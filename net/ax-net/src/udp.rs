//! UDP socket implementation.
//!
//! UDP sockets wrap smoltcp datagram sockets with POSIX-style bind/connect,
//! per-address port ownership, connected-peer filtering, route-aware source
//! selection, and MSG_MORE corking for datagram coalescing.
//!
//! # Bind And Routing Semantics
//!
//! Public binds are checked through `SocketSetWrapper` so wildcard and specific
//! address conflicts match Linux expectations. When a socket is connected or
//! sends to a destination, the control plane selects the source address and
//! device binding from the route table unless the socket was explicitly bound
//! to a concrete local address/interface.
//!
//! # Datagram Semantics
//!
//! smoltcp stores UDP payload plus metadata, while the POSIX surface exposes
//! per-call source addresses, `MSG_TRUNC`, `MSG_PEEK`, `MSG_DONTWAIT`, and
//! `MSG_MORE`. This module is responsible for preserving message boundaries and
//! for filtering connected sockets to their expected peer.
//!
//! # Polling
//!
//! UDP send/recv operations request the unique protocol executor after socket
//! state changes. They do not run the interface poll loop directly.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    task::Waker,
};

use ax_io::prelude::*;
use ax_sync::{Mutex, RawSpinLock};
use axpoll::{ExclusiveRegistrationSink, IoEvents, Pollable, SharedRegistrationSink};
use axpoll_set::PollSet;
use smoltcp::{
    iface::SocketHandle,
    phy::PacketMeta,
    socket::udp::{self as smol, UdpMetadata},
    storage::PacketMetadata,
    wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol},
};

use crate::{
    ConnectStatus, IpCmsg, NetError, NetResult, ReadinessVersion, RecvFlags, RecvOptions,
    SOCKET_SET, SendFlags, SendOptions, Shutdown, SocketAddrEx, SocketDeferPollWake, SocketOps,
    addr::allocate_ephemeral_port,
    config::{DeviceBinding, InterfaceId},
    consts::{UDP_RX_BUF_LEN, UDP_TX_BUF_LEN},
    flush_egress,
    general::GeneralOptions,
    get_control, interface_by_id,
    ip_tos::{EgressIpTosKey, clear_egress_ip_tos, set_egress_ip_tos},
    options::{
        Configurable, GetSocketOption, Ipv4MulticastInterface, Ipv4MulticastMembership,
        SetSocketOption,
    },
    request_poll,
    rx_meta::{ReceivedTrafficClass, received_traffic_class},
};

/// Buffered state for MSG_MORE corking: captures the target endpoint
/// and source address at the first MSG_MORE call so the merged datagram
/// is always delivered to the correct peer regardless of subsequent
/// calls' addresses.
struct CorkState {
    buf: Vec<u8>,
    remote: IpEndpoint,
    source: IpAddress,
}

#[derive(Default)]
struct Ipv4MulticastState {
    output: Ipv4MulticastInterface,
    memberships: Vec<(Ipv4Addr, InterfaceId)>,
}

/// A UDP socket that provides POSIX-like APIs.
pub struct UdpSocket {
    /// Handle into the global smoltcp socket set.
    handle: SocketHandle,
    /// Serializes the multi-step public bind transaction.
    bind_lock: Mutex<()>,
    /// Bound local endpoint as exposed by POSIX socket calls.
    local_addr: Mutex<Option<IpEndpoint>>,
    /// Connected remote endpoint plus selected source address.
    peer_addr: Mutex<Option<(IpEndpoint, IpAddress)>>,

    /// Shared socket options and blocking helpers.
    general: GeneralOptions,
    /// Multiplexes protocol and timer wakeups to owned poll registrations.
    poll_state: Arc<PollSet>,
    /// Generation published for each socket readiness wake.
    readiness_version: ReadinessVersion,
    /// Egress IP_TOS policies registered for recently used UDP destinations.
    tos_keys: RawSpinLock<Vec<EgressIpTosKey>>,
    /// MSG_MORE corking state: captures endpoint at first MSG_MORE
    /// so the merged datagram always goes to the correct peer.
    // Linux serializes UDP corking with the process-context socket lock. This
    // state may remain held while the global protocol socket is contended, so
    // it must be sleepable rather than an IRQ-disabling raw spin lock.
    cork: Mutex<Option<CorkState>>,
    /// Membership changes precede service/socket-set locks; the poller never
    /// takes this lock. Each entry owns one shared interface membership.
    multicast: Mutex<Ipv4MulticastState>,
}

impl UdpSocket {
    /// Creates a new UDP socket.
    pub fn new() -> Self {
        Self {
            handle: SOCKET_SET.add(smol::Socket::new(
                smol::PacketBuffer::new(vec![PacketMetadata::EMPTY; 256], vec![0; UDP_RX_BUF_LEN]),
                smol::PacketBuffer::new(vec![PacketMetadata::EMPTY; 256], vec![0; UDP_TX_BUF_LEN]),
            )),
            bind_lock: Mutex::new(()),
            local_addr: Mutex::new(None),
            peer_addr: Mutex::new(None),

            general: GeneralOptions::new(2, 2, 17), // SOCK_DGRAM
            poll_state: Arc::new(PollSet::new()),
            readiness_version: ReadinessVersion::new(),
            tos_keys: RawSpinLock::new(Vec::new()),
            cork: Mutex::new(None),
            multicast: Mutex::new(Ipv4MulticastState::default()),
        }
    }

    /// Returns the latest readiness wake generation for edge-triggered pollers.
    pub fn readiness_version(&self) -> u64 {
        self.readiness_version.current()
    }

    /// Restricts this socket to one interface for route selection.
    pub fn bind_device(&self, interface_id: InterfaceId) -> NetResult {
        if interface_by_id(interface_id).is_none() {
            return Err(NetError::NoSuchDevice);
        }
        self.general.set_device_binding(DeviceBinding {
            bound_if: Some(interface_id),
        });
        Ok(())
    }

    /// Borrows the underlying smoltcp UDP socket by handle.
    fn with_smol_socket<R>(&self, f: impl FnOnce(&mut smol::Socket) -> R) -> R {
        SOCKET_SET.with_socket_mut::<smol::Socket, _, _>(self.handle, f)
    }

    /// Returns the connected peer and cached source address.
    fn remote_endpoint(&self) -> NetResult<(IpEndpoint, IpAddress)> {
        let (endpoint, source) = match self.peer_addr.try_lock() {
            Some(addr) => addr.ok_or(NetError::NotConnected)?,
            None => return Err(NetError::NotConnected),
        };
        if endpoint.addr.is_multicast() {
            Ok((endpoint, self.send_source_for_remote(&endpoint.addr)?))
        } else {
            Ok((endpoint, source))
        }
    }

    /// Selects the source address used to reach `remote`.
    fn source_for_remote(&self, remote: &IpAddress) -> NetResult<IpAddress> {
        Ok(get_control()
            .select_route_with_binding(remote, self.general.device_binding())?
            .source)
    }

    fn send_source_for_remote(&self, remote: &IpAddress) -> NetResult<IpAddress> {
        if let IpAddress::Ipv4(group) = remote
            && group.is_multicast()
        {
            let output = self.multicast.lock().output;
            return self
                .resolve_multicast_interface(*group, output)
                .map(|(_, source)| source);
        }
        if let Some(local_ep) = *self.local_addr.lock()
            && !local_ep.addr.is_multicast()
            && !local_ep.addr.is_unspecified()
        {
            Ok(local_ep.addr)
        } else {
            self.source_for_remote(remote)
        }
    }

    fn source_and_binding_update_for_remote(
        &self,
        remote: &IpAddress,
    ) -> NetResult<(IpAddress, bool)> {
        if remote.is_multicast() {
            return Ok((self.send_source_for_remote(remote)?, false));
        }
        if let Some(local_ep) = *self.local_addr.lock()
            && !local_ep.addr.is_multicast()
            && !local_ep.addr.is_unspecified()
        {
            Ok((local_ep.addr, false))
        } else {
            Ok((self.source_for_remote(remote)?, true))
        }
    }

    fn track_egress_ip_tos(&self, local_addr: Option<IpAddress>, remote: IpEndpoint) {
        let tos = self.general.ip_tos();
        if tos == 0 {
            return;
        }
        let Some(local_addr) = local_addr else {
            return;
        };
        let Some(local) = self.local_addr.lock().map(|endpoint| IpEndpoint {
            addr: local_addr,
            port: endpoint.port,
        }) else {
            return;
        };
        let Some(key) = EgressIpTosKey::exact(IpProtocol::Udp, local, remote) else {
            return;
        };

        let mut keys = self.tos_keys.lock();
        if !keys.contains(&key) {
            keys.push(key);
        }
        set_egress_ip_tos(key, tos);
    }

    fn sync_tracked_egress_ip_tos(&self) {
        let tos = self.general.ip_tos();
        for key in self.tos_keys.lock().iter().copied() {
            set_egress_ip_tos(key, tos);
        }
    }

    fn clear_tracked_egress_ip_tos(&self) {
        for key in self.tos_keys.lock().drain(..) {
            clear_egress_ip_tos(key);
        }
    }

    fn resolve_multicast_interface(
        &self,
        group: Ipv4Addr,
        selection: Ipv4MulticastInterface,
    ) -> NetResult<(InterfaceId, IpAddress)> {
        let binding = self.general.device_binding();
        let selected = if let Some(interface_id) = selection.interface {
            Some(interface_id)
        } else if !selection.address.is_unspecified() {
            get_control()
                .local_binding_for(&IpListenEndpoint {
                    addr: Some(IpAddress::Ipv4(selection.address)),
                    port: 0,
                })?
                .bound_if
        } else {
            binding.bound_if
        };
        let interface_id = match selected {
            Some(interface_id) => interface_id,
            None => get_control().select_route(&IpAddress::Ipv4(group))?.interface_id,
        };
        if binding.bound_if.is_some_and(|bound| bound != interface_id) {
            return Err(NetError::InvalidInput);
        }
        let interface = interface_by_id(interface_id).ok_or(NetError::NoSuchDevice)?;
        let source = interface.ipv4.ok_or(NetError::NoSuchDeviceOrAddress)?.address.address();
        Ok((interface_id, IpAddress::Ipv4(source)))
    }

    fn add_ipv4_membership(&self, request: Ipv4MulticastMembership) -> NetResult {
        if !request.group.is_multicast() {
            return Err(NetError::InvalidInput);
        }
        let (interface_id, _) =
            self.resolve_multicast_interface(request.group, request.interface)?;
        let mut multicast = self.multicast.lock();
        let membership = (request.group, interface_id);
        if multicast.memberships.contains(&membership) {
            return Err(NetError::AddrInUse);
        }
        crate::get_service().join_ipv4_multicast(request.group, interface_id)?;
        multicast.memberships.push(membership);
        drop(multicast);
        request_poll();
        Ok(())
    }

    fn drop_ipv4_membership(&self, request: Ipv4MulticastMembership) -> NetResult {
        if !request.group.is_multicast() {
            return Err(NetError::InvalidInput);
        }
        let interface_id = if request.interface == Ipv4MulticastInterface::default() {
            None
        } else {
            Some(self.resolve_multicast_interface(request.group, request.interface)?.0)
        };
        let mut multicast = self.multicast.lock();
        let index = multicast.memberships.iter().position(|(group, id)| {
            *group == request.group && interface_id.is_none_or(|selected| selected == *id)
        }).ok_or(NetError::NoSuchDeviceOrAddress)?;
        let (group, interface_id) = multicast.memberships.swap_remove(index);
        crate::get_service().leave_ipv4_multicast(group, interface_id);
        drop(multicast);
        request_poll();
        Ok(())
    }

    fn leave_ipv4_memberships(&self) {
        let memberships = core::mem::take(&mut self.multicast.lock().memberships);
        if memberships.is_empty() {
            return;
        }
        {
            let mut service = crate::get_service();
            for (group, interface_id) in memberships {
                service.leave_ipv4_multicast(group, interface_id);
            }
        }
        request_poll();
    }
}

impl Configurable for UdpSocket {
    fn get_option_inner(&self, option: &mut GetSocketOption) -> NetResult<bool> {
        use GetSocketOption as O;

        if self.general.get_option_inner(option)? {
            return Ok(true);
        }
        match option {
            O::Ttl(ttl) => {
                self.with_smol_socket(|socket| {
                    **ttl = socket.hop_limit().unwrap_or(64);
                });
            }
            O::SendBuffer(size) => {
                **size = UDP_TX_BUF_LEN;
            }
            O::ReceiveBuffer(size) => {
                **size = UDP_RX_BUF_LEN;
            }
            O::Ipv4MulticastInterface(interface) => {
                **interface = self.multicast.lock().output;
            }
            O::Ipv4MulticastTtl(ttl) => {
                **ttl = self.with_smol_socket(|socket| socket.multicast_hop_limit());
            }
            O::Ipv4MulticastLoop(enabled) => {
                **enabled = self.with_smol_socket(|socket| socket.multicast_loop());
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> NetResult<bool> {
        use SetSocketOption as O;

        if let O::IpTos(tos) = option {
            self.general.set_ip_tos(*tos);
            self.sync_tracked_egress_ip_tos();
            return Ok(true);
        }

        if self.general.set_option_inner(option)? {
            return Ok(true);
        }
        match option {
            O::Ttl(ttl) => {
                self.with_smol_socket(|socket| {
                    socket.set_hop_limit(Some(*ttl));
                });
            }
            O::Ipv4MulticastInterface(interface) => {
                if *interface != Ipv4MulticastInterface::default() {
                    self.resolve_multicast_interface(Ipv4Addr::new(224, 0, 0, 1), *interface)?;
                }
                self.multicast.lock().output = *interface;
            }
            O::Ipv4MulticastTtl(ttl) => {
                self.with_smol_socket(|socket| socket.set_multicast_hop_limit(*ttl));
            }
            O::Ipv4MulticastLoop(enabled) => {
                self.with_smol_socket(|socket| socket.set_multicast_loop(*enabled));
            }
            O::AddIpv4Membership(request) => self.add_ipv4_membership(*request)?,
            O::DropIpv4Membership(request) => self.drop_ipv4_membership(*request)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}
impl SocketOps for UdpSocket {
    /// Binds the UDP socket and records public port ownership.
    fn bind(&self, local_addr: SocketAddrEx) -> NetResult {
        let mut local_addr = local_addr.into_ip()?;
        let _bind_guard = self.bind_lock.lock();

        if self.local_addr.lock().is_some() {
            return Err(NetError::InvalidInput);
        }
        if local_addr.port() == 0 {
            local_addr.set_port(get_ephemeral_port()?);
        }

        let local_endpoint = IpEndpoint::from(local_addr);
        let endpoint = IpListenEndpoint {
            addr: (!local_endpoint.addr.is_unspecified()).then_some(local_endpoint.addr),
            port: local_endpoint.port,
        };
        let binding = if endpoint.addr.is_some_and(|address| address.is_multicast()) {
            DeviceBinding::default()
        } else {
            get_control().local_binding_for(&endpoint)?
        };

        self.with_smol_socket(|socket| {
            socket.bind(endpoint).map_err(|e| match e {
                smol::BindError::InvalidState => NetError::InvalidInput,
                smol::BindError::Unaddressable => NetError::ConnectionRefused,
            })
        })?;
        if let Err(err) = SOCKET_SET.udp_bind(
            self.handle,
            local_endpoint.addr,
            local_endpoint.port,
            self.general.reuse_port(),
            self.general.reuse_address(),
        ) {
            self.with_smol_socket(|socket| socket.close());
            return Err(err);
        }
        if binding.bound_if.is_some() {
            self.general.set_device_binding(binding);
        }

        *self.local_addr.lock() = Some(local_endpoint);
        info!("UDP socket {}: bound on {}", self.handle, endpoint);
        Ok(())
    }

    /// Stores a default peer and source address for connected UDP semantics.
    fn start_connect(&self, remote_addr: SocketAddrEx) -> NetResult<ConnectStatus> {
        let remote_addr = remote_addr.into_ip()?;
        let mut guard = self.peer_addr.lock();

        if self.local_addr.lock().is_none() {
            self.bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )))?;
        }

        let remote_addr = IpEndpoint::from(remote_addr);
        let local_port = self.local_addr.lock().map_or(0, |endpoint| endpoint.port);
        let (src, should_update_binding) =
            self.source_and_binding_update_for_remote(&remote_addr.addr)?;

        *guard = Some((remote_addr, src));

        if should_update_binding {
            self.general
                .set_device_binding(get_control().local_binding_for(&IpListenEndpoint {
                    addr: Some(src),
                    port: local_port,
                })?);
        }

        debug!("UDP socket {}: connected to {}", self.handle, remote_addr);
        Ok(ConnectStatus::Connected)
    }

    /// Sends one datagram, or appends to/flushed a MSG_MORE corked datagram.
    fn try_send(&self, mut src: impl Read + IoBuf, options: &mut SendOptions) -> NetResult<usize> {
        // MSG_OOB is only valid on stream sockets (SOCK_STREAM), not DGRAM.
        if options.flags.contains(SendFlags::OOB) {
            return Err(NetError::OperationNotSupported);
        }

        if self.local_addr.lock().is_none() {
            self.bind(SocketAddrEx::Ip(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                0,
            )))?;
        }
        // MSG_MORE corking: buffer data instead of sending immediately.
        // Cap corked data to the UDP TX buffer size to prevent
        // unbounded kernel memory allocation from user input.
        const CORK_MAX: usize = UDP_TX_BUF_LEN;
        let more = options.flags.contains(SendFlags::MORE);

        if more {
            let (remote_addr, source_addr) = match options.to.clone() {
                Some(addr) => {
                    let addr = IpEndpoint::from(addr.into_ip()?);
                    let src = self.send_source_for_remote(&addr.addr)?;
                    (addr, src)
                }
                None => match self.remote_endpoint() {
                    Ok((endpoint, src)) => (endpoint, src),
                    Err(_) => return Err(NetError::DestAddrRequired),
                },
            };
            if remote_addr.port == 0 || remote_addr.addr.is_unspecified() {
                return Err(NetError::InvalidInput);
            }
            let len = src.remaining();
            if len > CORK_MAX {
                return Err(NetError::MessageTooLong);
            }
            let mut tmp = alloc::vec![0u8; len];
            let read = src.read(&mut tmp)?;
            let mut cork = self.cork.lock();
            if cork.is_none() {
                *cork = Some(CorkState {
                    buf: tmp[..read].to_vec(),
                    remote: remote_addr,
                    source: source_addr,
                });
            } else {
                let prev = cork.as_ref().unwrap().buf.len();
                let new_len = prev.checked_add(read).ok_or(NetError::MessageTooLong)?;
                if new_len > CORK_MAX {
                    return Err(NetError::MessageTooLong);
                }
                cork.as_mut().unwrap().buf.extend_from_slice(&tmp[..read]);
            }
            return Ok(read);
        }

        // Resolve destination for direct send or cork flush.
        // None means unconnected socket without explicit destination;
        // the poller closure checks cork before demanding an address.
        let resolved = match options.to.clone() {
            Some(addr) => {
                let addr = IpEndpoint::from(addr.into_ip()?);
                let src = self.send_source_for_remote(&addr.addr)?;
                Some((addr, src))
            }
            None => self.remote_endpoint().ok(),
        };

        request_poll();
        let mut cork_guard = self.cork.lock();
        // When flushing corked data, always use the endpoint captured
        // at the first MSG_MORE call (matching Linux semantics).
        let (endpoint, local_addr, payload_len) = if let Some(ref c) = *cork_guard {
            let total = c
                .buf
                .len()
                .checked_add(src.remaining())
                .ok_or(NetError::MessageTooLong)?;
            if total > CORK_MAX {
                return Err(NetError::MessageTooLong);
            }
            (c.remote, Some(c.source), total)
        } else {
            match resolved {
                Some((remote, source)) => {
                    if remote.port == 0 || remote.addr.is_unspecified() {
                        return Err(NetError::InvalidInput);
                    }
                    (remote, Some(source), src.remaining())
                }
                None => return Err(NetError::DestAddrRequired),
            }
        };
        let result = self.with_smol_socket(|socket| {
            if !socket.is_open() {
                // not connected
                Err(NetError::NotConnected)
            } else if !socket.can_send() {
                Err(NetError::WouldBlock)
            } else {
                self.track_egress_ip_tos(local_addr, endpoint);
                // UDP allows zero-length payloads (IP header + UDP header only).
                if payload_len == 0 {
                    socket
                        .send(
                            0,
                            UdpMetadata {
                                endpoint,
                                local_address: local_addr,
                                meta: PacketMeta::default(),
                            },
                        )
                        .map_err(|e| match e {
                            smol::SendError::BufferFull => NetError::WouldBlock,
                            smol::SendError::Unaddressable => NetError::ConnectionRefused,
                        })?;
                    *cork_guard = None;
                    return Ok(0);
                }
                let buf = socket
                    .send(
                        payload_len,
                        UdpMetadata {
                            endpoint,
                            local_address: local_addr,
                            meta: PacketMeta::default(),
                        },
                    )
                    .map_err(|e| match e {
                        smol::SendError::BufferFull => NetError::WouldBlock,
                        smol::SendError::Unaddressable => NetError::ConnectionRefused,
                    })?;
                let mut total_written = 0;
                let mut cur_read = 0;
                if let Some(ref c) = *cork_guard {
                    let n = c.buf.len().min(buf.len());
                    buf[..n].copy_from_slice(&c.buf[..n]);
                    total_written += n;
                }
                if total_written < buf.len() {
                    cur_read = src.read(&mut buf[total_written..])?;
                    total_written += cur_read;
                }
                assert_eq!(total_written, buf.len());
                // Success — clear cork state.
                *cork_guard = None;
                // Return only bytes consumed from the *current* user buffer.
                Ok(cur_read)
            }
        })?;
        request_poll();
        Ok(result)
    }

    /// Receives one datagram while honoring peer filters and recv flags.
    fn try_recv(&self, mut dst: impl Write, options: &mut RecvOptions) -> NetResult<usize> {
        enum ExpectedRemote<'a> {
            Any(&'a mut SocketAddrEx),
            AnyDiscard,
            Expecting(IpEndpoint),
        }
        let mut expected_remote = match options.from.as_deref_mut() {
            Some(addr) => ExpectedRemote::Any(addr),
            None => match self.remote_endpoint() {
                Ok((endpoint, _)) => ExpectedRemote::Expecting(endpoint),
                Err(_) => ExpectedRemote::AnyDiscard,
            },
        };

        request_poll();
        self.with_smol_socket(|socket| {
            if !socket.can_recv() {
                Err(NetError::WouldBlock)
            } else {
                let result = if options.flags.contains(RecvFlags::PEEK) {
                    socket.peek().map(|(data, meta)| (data, *meta))
                } else {
                    socket.recv()
                };
                match result {
                    Ok((src, meta)) => {
                        match &mut expected_remote {
                            ExpectedRemote::Any(remote_addr) => {
                                **remote_addr = SocketAddrEx::Ip(meta.endpoint.into());
                            }
                            ExpectedRemote::AnyDiscard => {
                                // recv() with no addr buffer and no peer — accept from any
                            }
                            ExpectedRemote::Expecting(expected) => {
                                if (!expected.addr.is_unspecified()
                                    && expected.addr != meta.endpoint.addr)
                                    || (expected.port != 0 && expected.port != meta.endpoint.port)
                                {
                                    return Err(NetError::WouldBlock);
                                }
                            }
                        }

                        let read = dst.write(src)?;
                        if read < src.len() {
                            warn!("UDP message truncated: {} -> {} bytes", src.len(), read);
                            if let Some(ref mut truncated) = options.truncated {
                                **truncated = true;
                            }
                        }

                        if let Some(cmsg) = options.cmsg.as_deref_mut()
                            && let Some(traffic_class) = received_traffic_class(meta.meta)
                        {
                            match traffic_class {
                                ReceivedTrafficClass::Ipv4(tos)
                                    if self.general.recv_traffic_class() =>
                                {
                                    cmsg.push(Box::new(IpCmsg::Ipv6TrafficClass(tos)));
                                }
                                ReceivedTrafficClass::Ipv4(tos) if self.general.recv_tos() => {
                                    cmsg.push(Box::new(IpCmsg::Ipv4Tos(tos)));
                                }
                                ReceivedTrafficClass::Ipv6(tclass)
                                    if self.general.recv_traffic_class() =>
                                {
                                    cmsg.push(Box::new(IpCmsg::Ipv6TrafficClass(tclass)));
                                }
                                _ => {}
                            }
                        }

                        Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                            src.len()
                        } else {
                            read
                        })
                    }
                    Err(smol::RecvError::Exhausted) => Err(NetError::WouldBlock),
                    Err(smol::RecvError::Truncated) => {
                        unreachable!("UDP socket recv never returns Err(Truncated)")
                    }
                }
            }
        })
    }

    fn local_addr(&self) -> NetResult<SocketAddrEx> {
        match self.local_addr.try_lock() {
            Some(addr) => addr
                .map(Into::into)
                .map(SocketAddrEx::Ip)
                .ok_or(NetError::NotConnected),
            None => Err(NetError::NotConnected),
        }
    }

    fn peer_addr(&self) -> NetResult<SocketAddrEx> {
        self.remote_endpoint()
            .map(|it| it.0.into())
            .map(SocketAddrEx::Ip)
    }

    fn shutdown(&self, _how: Shutdown) -> NetResult {
        // TODO(mivik): shutdown
        request_poll();

        self.with_smol_socket(|socket| {
            debug!("UDP socket {}: shutting down", self.handle);
            socket.close();
        });
        Ok(())
    }
}

impl Pollable for UdpSocket {
    fn poll(&self) -> IoEvents {
        request_poll();
        let Some(local_addr) = self.local_addr.try_lock() else {
            return IoEvents::empty();
        };
        if local_addr.is_none() {
            return IoEvents::empty();
        }
        drop(local_addr);

        let mut events = IoEvents::empty();
        self.with_smol_socket(|socket| {
            events.set(IoEvents::IN, socket.can_recv());
            events.set(IoEvents::OUT, socket.can_send());
        });
        events
    }

    unsafe fn register_shared(&self, sink: &mut dyn SharedRegistrationSink, events: IoEvents) {
        unsafe { sink.register_shared(&self.poll_state, events) };
        self.arm_poll_sources(events);
    }

    unsafe fn register_exclusive(
        &self,
        sink: &mut dyn ExclusiveRegistrationSink,
        events: IoEvents,
    ) {
        unsafe { sink.register_exclusive(&self.poll_state, events) };
        self.arm_poll_sources(events);
    }
}

impl UdpSocket {
    fn arm_poll_sources(&self, events: IoEvents) {
        self.with_smol_socket(|socket| {
            if events.contains(IoEvents::IN) {
                socket.register_recv_waker(&Waker::from(Arc::new(SocketDeferPollWake::new(
                    self.poll_state.clone(),
                    IoEvents::IN,
                    self.readiness_version.clone(),
                ))));
            }
            if events.contains(IoEvents::OUT) {
                socket.register_send_waker(&Waker::from(Arc::new(SocketDeferPollWake::new(
                    self.poll_state.clone(),
                    IoEvents::OUT,
                    self.readiness_version.clone(),
                ))));
            }
        });
        if events.intersects(IoEvents::IN | IoEvents::OUT) {
            self.general
                .register_waker(&Waker::from(Arc::new(SocketDeferPollWake::new(
                    self.poll_state.clone(),
                    events,
                    self.readiness_version.clone(),
                ))));
        }
    }
}

impl Default for UdpSocket {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        // Dispatch any datagram still queued in the TX buffer to its destination
        // while the socket is still open, so a send immediately followed by close
        // is not lost (Linux keeps the datagram in the peer's receive buffer).
        // smoltcp's `close()` drops the send buffer, so this must run before it.
        flush_egress();
        self.shutdown(Shutdown::Both).ok();
        self.leave_ipv4_memberships();
        self.clear_tracked_egress_ip_tos();
        SOCKET_SET.remove(self.handle);
    }
}

fn get_ephemeral_port() -> NetResult<u16> {
    allocate_ephemeral_port(|port| {
        SOCKET_SET.udp_port_available(IpAddress::Ipv4(Ipv4Addr::UNSPECIFIED), port)
    })
}
