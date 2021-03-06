// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under the MIT license <LICENSE-MIT
// http://opensource.org/licenses/MIT> or the Modified BSD license <LICENSE-BSD
// https://opensource.org/licenses/BSD-3-Clause>, at your option. This file may not be copied,
// modified, or distributed except according to those terms. Please review the Licences for the
// specific language governing permissions and limitations relating to use of the SAFE Network
// Software.

use crate::common::{
    ipv4_addr, BootstrapDenyReason, BootstrapperRole, CoreTimer, CrustUser, Message, NameHash,
    PeerInfo, State,
};
use crate::main::{
    read_config_file, ActiveConnection, Config, ConnectionCandidate, ConnectionId, CrustData,
    Event, EventLoopCore,
};
use crate::nat::{ip_addr_is_global, GetExtAddr};
use crate::PeerId;
use mio::{Poll, PollOpt, Ready, Token};
use mio_extras::timer::Timeout;
use safe_crypto::{PublicEncryptKey, SecretEncryptKey};
use socket_collection::{DecryptContext, EncryptContext, Priority, TcpSock};
use std::any::Any;
use std::cell::{RefCell, RefMut};
use std::collections::hash_map::Entry;
use std::collections::HashSet;
use std::mem;
use std::net::SocketAddr;
use std::rc::{Rc, Weak};
use std::time::Duration;

pub const EXCHANGE_MSG_TIMEOUT_SEC: u64 = 10 * 60;
const CHECK_REACHABILITY_TIMEOUT_SEC: u64 = 3;

/// Remote peer might send a huge list of external addresses to test for reachability. That
/// would result in a lot of requests being made.
/// Under normal circumstances that wouldn't happen, unless some malicious peer was trying to do
/// denial of service attack. Hence, limit the number of addresses we use for external reachability
/// test.
const MAX_EXT_REACHABILITY_TEST_ADDRS: usize = 3;

/// Handles incoming socket according to the first received request.
pub struct ExchangeMsg {
    token: Token,
    event_tx: crate::CrustEventSender,
    name_hash: NameHash,
    next_state: NextState,
    our_uid: PeerId,
    socket: TcpSock,
    timeout: Timeout,
    reachability_children: HashSet<Token>,
    accept_bootstrap: bool,
    test_ext_reachability: bool,
    self_weak: Weak<RefCell<ExchangeMsg>>,
    our_sk: SecretEncryptKey,
}

impl ExchangeMsg {
    /// # Args
    ///
    /// `test_ext_reachability` - if true, we will check if remote peer has public IP and we can
    ///     reach it directly.
    pub fn start(
        core: &mut EventLoopCore,
        poll: &Poll,
        timeout_sec: Option<u64>,
        socket: TcpSock,
        accept_bootstrap: bool,
        our_uid: PeerId,
        name_hash: NameHash,
        event_tx: crate::CrustEventSender,
        our_sk: &SecretEncryptKey,
        test_ext_reachability: bool,
    ) -> crate::Res<()> {
        let token = core.get_new_token();

        let kind = Ready::readable();
        poll.register(&socket, token, kind, PollOpt::edge())?;

        let timeout = core.set_timeout(
            Duration::from_secs(timeout_sec.unwrap_or(EXCHANGE_MSG_TIMEOUT_SEC)),
            CoreTimer::new(token, 0),
        );

        let state = Rc::new(RefCell::new(Self {
            token,
            event_tx,
            name_hash,
            next_state: NextState::None,
            our_uid,
            socket,
            timeout,
            reachability_children: HashSet::with_capacity(4),
            accept_bootstrap,
            test_ext_reachability,
            self_weak: Default::default(),
            our_sk: our_sk.clone(),
        }));

        state.borrow_mut().self_weak = Rc::downgrade(&state);

        let _ = core.insert_state(token, state);

        Ok(())
    }

    fn read(&mut self, core: &mut EventLoopCore, poll: &Poll) {
        match self.socket.read::<Message>() {
            Ok(Some(Message::BootstrapRequest(their_uid, name_hash, their_role))) => {
                if !self.accept_bootstrap {
                    debug!("Bootstrapping off us is not allowed");
                    return self.terminate(core, poll);
                }

                match self.validate_peer_uid(their_uid) {
                    Ok(their_uid) => {
                        self.handle_bootstrap_req(core, poll, their_uid, name_hash, their_role)
                    }
                    Err(()) => self.terminate(core, poll),
                }
            }
            Ok(Some(Message::ConnectRequest(their_uid, name_hash, their_addrs))) => {
                match self.validate_peer_uid(their_uid) {
                    Ok(their_uid) => {
                        self.handle_connect(core, poll, their_uid, name_hash, their_addrs)
                    }
                    Err(()) => self.terminate(core, poll),
                }
            }
            Ok(Some(Message::EchoAddrReq(their_pk))) => {
                self.handle_echo_addr_req(core, poll, their_pk)
            }
            Ok(Some(message)) => {
                debug!("Unexpected message in direct connect: {:?}", message);
                self.terminate(core, poll)
            }
            Ok(None) => (),
            Err(e) => {
                debug!("Failed to read from socket: {:?}", e);
                self.terminate(core, poll);
            }
        }
    }

    fn handle_bootstrap_req(
        &mut self,
        core: &mut EventLoopCore,
        poll: &Poll,
        their_uid: PeerId,
        name_hash: NameHash,
        their_role: BootstrapperRole,
    ) {
        if !self.is_valid_name_hash(name_hash) {
            debug!("Rejecting Bootstrapper with an invalid name hash.");
            return self.write(
                core,
                poll,
                Some((
                    Message::BootstrapDenied(BootstrapDenyReason::InvalidNameHash),
                    0,
                )),
            );
        }

        if !self.use_authed_encryption(their_uid.pub_enc_key) {
            debug!("Failed to set authenticated encryption context.");
            return self.terminate(core, poll);
        }

        self.try_update_crust_config(core);

        if !self.is_peer_whitelisted((&their_role).into(), &core.user_data().config.cfg) {
            debug!("Bootstrapper is not whitelisted. Denying bootstrap.");
            let reason = match their_role {
                BootstrapperRole::Node(_) => BootstrapDenyReason::NodeNotWhitelisted,
                BootstrapperRole::Client => BootstrapDenyReason::ClientNotWhitelisted,
            };
            return self.write(core, poll, Some((Message::BootstrapDenied(reason), 0)));
        }

        if let BootstrapperRole::Node(their_addrs) = their_role {
            if self.test_ext_reachability {
                let on_check_reachability_result =
                    |mut state: RefMut<ExchangeMsg>,
                     core: &mut EventLoopCore,
                     poll: &Poll,
                     child,
                     res| {
                        state.handle_check_reachability(core, poll, child, res);
                    };
                self.spawn_ext_reachability_tests(
                    core,
                    poll,
                    their_uid,
                    their_addrs,
                    on_check_reachability_result,
                );
                if self.reachability_children.is_empty() {
                    debug!(
                        "Bootstrapper failed to pass requisite condition of external \
                         reachability. Denying bootstrap."
                    );
                    let reason = BootstrapDenyReason::FailedExternalReachability;
                    self.write(core, poll, Some((Message::BootstrapDenied(reason), 0)));
                }
            } else {
                self.send_bootstrap_grant(core, poll, their_uid, CrustUser::Node)
            }
        } else {
            self.send_bootstrap_grant(core, poll, their_uid, CrustUser::Client)
        }
    }

    fn is_peer_whitelisted(&self, peer_kind: CrustUser, config: &Config) -> bool {
        let peer_ip = match self.socket.peer_addr() {
            Ok(s) => s.ip(),
            Err(e) => {
                debug!(
                    "Could not obtain IP Address of peer: {:?}. Denying handshake.",
                    e
                );
                return false;
            }
        };

        let res = match peer_kind {
            CrustUser::Node => config
                .whitelisted_node_ips
                .as_ref()
                .map_or(true, |ips| ips.contains(&peer_ip)),
            CrustUser::Client => config
                .whitelisted_client_ips
                .as_ref()
                .map_or(true, |ips| ips.contains(&peer_ip)),
        };

        if !res {
            trace!("IP: {} is not whitelisted.", peer_ip);
        }

        res
    }

    fn handle_check_reachability(
        &mut self,
        core: &mut EventLoopCore,
        poll: &Poll,
        child: Token,
        res: Result<PeerId, ()>,
    ) {
        let _ = self.reachability_children.remove(&child);
        if let Ok(their_uid) = res {
            self.terminate_childern(core, poll);
            return self.send_bootstrap_grant(core, poll, their_uid, CrustUser::Node);
        }
        if self.reachability_children.is_empty() {
            debug!(
                "Bootstrapper failed to pass requisite condition of external recheability. \
                 Denying bootstrap."
            );
            let reason = BootstrapDenyReason::FailedExternalReachability;
            self.write(core, poll, Some((Message::BootstrapDenied(reason), 0)));
        }
    }

    fn send_bootstrap_grant(
        &mut self,
        core: &mut EventLoopCore,
        poll: &Poll,
        their_uid: PeerId,
        peer_kind: CrustUser,
    ) {
        self.enter_handshaking_mode(core, their_uid);

        let our_uid = self.our_uid;
        self.next_state = NextState::ActiveConnection(their_uid, peer_kind);
        self.write(core, poll, Some((Message::BootstrapGranted(our_uid), 0)))
    }

    fn handle_connect(
        &mut self,
        core: &mut EventLoopCore,
        poll: &Poll,
        their_uid: PeerId,
        name_hash: NameHash,
        their_addrs: HashSet<SocketAddr>,
    ) {
        if !self.is_valid_name_hash(name_hash) {
            debug!("Invalid name hash given. Denying connection.");
            return self.terminate(core, poll);
        }

        self.try_update_crust_config(core);

        if !self.is_peer_whitelisted(CrustUser::Node, &core.user_data().config.cfg) {
            debug!("Connecting Node is not whitelisted. Denying connection.");
            return self.terminate(core, poll);
        }

        if !self.use_authed_encryption(their_uid.pub_enc_key) {
            debug!("Failed to set authenticated encryption context.");
            return self.terminate(core, poll);
        }

        if self.test_ext_reachability {
            let on_check_reachability_result =
                |mut state: RefMut<ExchangeMsg>,
                 core: &mut EventLoopCore,
                 poll: &Poll,
                 child,
                 res| {
                    state.handle_check_reachability_on_connect(core, poll, child, res);
                };
            self.spawn_ext_reachability_tests(
                core,
                poll,
                their_uid,
                their_addrs,
                on_check_reachability_result,
            );
            if self.reachability_children.is_empty() {
                debug!("External reachability test failed. Denying connect request.");
                let reason = BootstrapDenyReason::FailedExternalReachability;
                self.write(core, poll, Some((Message::BootstrapDenied(reason), 0)));
            }
        } else {
            self.send_connect_grant(core, poll, their_uid);
        }
    }

    /// Called when external reachability test passed.
    fn handle_check_reachability_on_connect(
        &mut self,
        core: &mut EventLoopCore,
        poll: &Poll,
        child: Token,
        res: Result<PeerId, ()>,
    ) {
        let _ = self.reachability_children.remove(&child);
        if let Ok(their_uid) = res {
            self.terminate_childern(core, poll);
            return self.send_connect_grant(core, poll, their_uid);
        }
        if self.reachability_children.is_empty() {
            trace!("External reachability test failed, terminating connection.");
            self.terminate(core, poll);
        }
    }

    /// Sends response to incoming connection.
    fn send_connect_grant(&mut self, core: &mut EventLoopCore, poll: &Poll, their_uid: PeerId) {
        self.enter_handshaking_mode(core, their_uid);
        self.next_state = NextState::ConnectionCandidate(their_uid);
        let msg = Message::ConnectResponse(self.our_uid, self.name_hash);
        self.write(core, poll, Some((msg, 0)));
    }

    fn handle_echo_addr_req(
        &mut self,
        core: &mut EventLoopCore,
        poll: &Poll,
        their_pk: PublicEncryptKey,
    ) {
        self.next_state = NextState::None;
        match (
            self.use_authed_encryption(their_pk),
            self.socket.peer_addr(),
        ) {
            (true, Ok(peer_addr)) => {
                self.write(core, poll, Some((Message::EchoAddrResp(peer_addr), 0)));
            }
            _ => self.terminate(core, poll),
        }
    }

    /// Set socket encrypt context to authenticated encryption.
    /// Returns false on failure.
    fn use_authed_encryption(&mut self, their_pk: PublicEncryptKey) -> bool {
        let shared_key = self.our_sk.shared_secret(&their_pk);
        match (
            self.socket
                .set_encrypt_ctx(EncryptContext::authenticated(shared_key.clone())),
            self.socket
                .set_decrypt_ctx(DecryptContext::authenticated(shared_key)),
        ) {
            (Ok(_), Ok(_)) => true,
            res => {
                debug!("Failed to set decrypt/encrypt context: {:?}", res);
                false
            }
        }
    }

    /// Spawns multiple children states that check which of given addresses are publicly reachable.
    fn spawn_ext_reachability_tests<F>(
        &mut self,
        core: &mut EventLoopCore,
        poll: &Poll,
        their_uid: PeerId,
        their_addrs: HashSet<SocketAddr>,
        on_result: F,
    ) where
        F: 'static
            + FnMut(RefMut<ExchangeMsg>, &mut EventLoopCore, &Poll, Token, Result<PeerId, ()>)
            + Clone,
    {
        for their_listener in self.addrs_for_ext_reachability_test(their_addrs) {
            let self_weak = self.self_weak.clone();
            let mut on_result = on_result.clone();
            let finish =
                move |core: &mut EventLoopCore, poll: &Poll, child, res: Result<SocketAddr, ()>| {
                    if let Some(self_rc) = self_weak.upgrade() {
                        let res = res.map(|_| their_uid);
                        on_result(self_rc.borrow_mut(), core, poll, child, res);
                    }
                };

            if let Ok(child) = GetExtAddr::<CrustData>::start(
                core,
                poll,
                ipv4_addr(0, 0, 0, 0, 0),
                &PeerInfo::new(their_listener, their_uid.pub_enc_key),
                self.our_uid.pub_enc_key,
                &self.our_sk,
                Some(CHECK_REACHABILITY_TIMEOUT_SEC),
                Box::new(finish),
            ) {
                let _ = self.reachability_children.insert(child);
            }
        }
    }

    /// Filters out addresses that are not public IPs and don't belong to the sender socket.
    fn addrs_for_ext_reachability_test(
        &self,
        their_addrs: HashSet<SocketAddr>,
    ) -> HashSet<SocketAddr> {
        their_addrs
            .into_iter()
            .filter(|addr| ip_addr_is_global(&addr.ip()))
            .filter(|addr| match self.socket.peer_addr() {
                Ok(peer_addr) => addr.ip() == peer_addr.ip(),
                Err(e) => {
                    debug!("Could not obtain IP Address of peer: {:?}.", e);
                    false
                }
            })
            .take(MAX_EXT_REACHABILITY_TEST_ADDRS)
            .collect()
    }

    fn enter_handshaking_mode(&self, core: &mut EventLoopCore, their_uid: PeerId) {
        let connections = &mut core.user_data_mut().connections;
        connections
            .entry(their_uid)
            .or_insert(ConnectionId {
                active_connection: None,
                currently_handshaking: 0,
            })
            .currently_handshaking += 1;
        trace!(
            "Connection Map inserted: {:?} -> {:?}",
            their_uid,
            connections.get(&their_uid)
        );
    }

    fn is_valid_name_hash(&self, name_hash: NameHash) -> bool {
        self.name_hash == name_hash
    }

    fn validate_peer_uid(&self, their_uid: PeerId) -> Result<PeerId, ()> {
        if self.our_uid == their_uid {
            debug!("Accepted connection from ourselves");
            return Err(());
        }

        Ok(their_uid)
    }

    fn try_update_crust_config(&self, core: &mut EventLoopCore) {
        match read_config_file() {
            Ok(cfg) => core
                .user_data_mut()
                .config
                .check_for_update_and_mark_modified(cfg),
            Err(e) => debug!("Could not read Crust config file: {:?}", e),
        }
    }

    fn write(&mut self, core: &mut EventLoopCore, poll: &Poll, msg: Option<(Message, Priority)>) {
        // Do not accept multiple bootstraps from same peer
        if let NextState::ActiveConnection(their_uid, _) = self.next_state {
            let terminate = match core.user_data().connections.get(&their_uid).cloned() {
                Some(ConnectionId {
                    active_connection: Some(_),
                    ..
                }) => true,
                _ => false,
            };
            if terminate {
                return self.terminate(core, poll);
            }
        }

        match self.socket.write(msg) {
            Ok(true) => self.done(core, poll),
            Ok(false) => (),
            Err(e) => {
                debug!("Error in writting: {:?}", e);
                self.terminate(core, poll)
            }
        }
    }

    fn done(&mut self, core: &mut EventLoopCore, poll: &Poll) {
        let _ = core.remove_state(self.token);
        let _ = core.cancel_timeout(&self.timeout);

        let our_uid = self.our_uid;
        let event_tx = self.event_tx.clone();

        match self.next_state {
            NextState::ActiveConnection(their_uid, peer_kind) => {
                let socket = mem::replace(&mut self.socket, Default::default());
                ActiveConnection::start(
                    core,
                    poll,
                    self.token,
                    socket,
                    our_uid,
                    their_uid,
                    peer_kind,
                    Event::BootstrapAccept(their_uid, peer_kind),
                    event_tx,
                );
            }
            NextState::ConnectionCandidate(their_uid) => {
                let handler = move |core: &mut EventLoopCore, poll: &Poll, token, res| {
                    if let Some(socket) = res {
                        ActiveConnection::start(
                            core,
                            poll,
                            token,
                            socket,
                            our_uid,
                            their_uid,
                            // Note; We enter ConnectionCandidate only with
                            //       Nodes
                            CrustUser::Node,
                            Event::ConnectSuccess(their_uid),
                            event_tx.clone(),
                        );
                    }
                };

                let socket = mem::replace(&mut self.socket, Default::default());
                let _ = ConnectionCandidate::start(
                    core,
                    poll,
                    self.token,
                    socket,
                    our_uid,
                    their_uid,
                    Box::new(handler),
                );
            }
            NextState::None => self.terminate(core, poll),
        }
    }

    fn terminate_childern(&mut self, core: &mut EventLoopCore, poll: &Poll) {
        for child in self.reachability_children.drain() {
            core.get_state(child)
                .map_or((), |c| c.borrow_mut().terminate(core, poll));
        }
    }
}

impl State<CrustData> for ExchangeMsg {
    fn ready(&mut self, core: &mut EventLoopCore, poll: &Poll, kind: Ready) {
        if kind.is_readable() {
            self.read(core, poll)
        }
        if kind.is_writable() {
            self.write(core, poll, None)
        }
    }

    fn terminate(&mut self, core: &mut EventLoopCore, poll: &Poll) {
        self.terminate_childern(core, poll);
        let _ = core.remove_state(self.token);

        match self.next_state {
            NextState::ConnectionCandidate(their_uid)
            | NextState::ActiveConnection(their_uid, _) => {
                let connections = &mut core.user_data_mut().connections;
                if let Entry::Occupied(mut oe) = connections.entry(their_uid) {
                    oe.get_mut().currently_handshaking -= 1;
                    if oe.get().currently_handshaking == 0 && oe.get().active_connection.is_none() {
                        let _ = oe.remove();
                    }
                }
                trace!(
                    "Connection Map removed: {:?} -> {:?}",
                    their_uid,
                    connections.get(&their_uid)
                );
            }
            NextState::None => (),
        }

        let _ = core.cancel_timeout(&self.timeout);
        let _ = poll.deregister(&self.socket);
    }

    fn timeout(&mut self, core: &mut EventLoopCore, poll: &Poll, _timer_id: u8) {
        debug!("Exchange message timed out. Terminating direct connection request.");
        self.terminate(core, poll)
    }

    fn as_any(&mut self) -> &mut Any {
        self
    }
}

enum NextState {
    None,
    ActiveConnection(PeerId, CrustUser),
    ConnectionCandidate(PeerId),
}
