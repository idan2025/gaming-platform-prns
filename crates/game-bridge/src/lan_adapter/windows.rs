//! The Windows room adapter: Wintun (`PLAN.md` §14, step 4).
//!
//! Wintun is WireGuard's layer-3 adapter for Windows: `wintun.dll`, signed by
//! WireGuard LLC, installs its own driver on first use and shipped unmodified
//! beside this binary under its prebuilt-binaries license (redistribution is
//! allowed alongside software that uses only the API in `wintun.h`, which is
//! all this file calls). It is loaded at runtime from an absolute path, never
//! by search order, so a planted DLL elsewhere on the path is never picked up.
//!
//! **Only an administrator can open a Wintun adapter**, which is where Windows
//! parts from Linux: `lan-helper serve` runs elevated, holds the adapter for the
//! whole session, and relays its packets to the unprivileged launcher
//! (`lan_relay.rs`). The adapter goes away when the helper closes it — when the
//! launcher's connection drops, however the launcher ended.
//!
//! # The metric is the fix
//!
//! Windows sends a broadcast to `255.255.255.255` out of the interface whose
//! route to it is cheapest, and every interface has one. On a machine with
//! Ethernet or Wi-Fi that is never a new adapter — which is why Hamachi users
//! were told to lower its metric by hand, and why an old game on a virtual LAN
//! so often "finds no games". [`configure`] sets this adapter's interface metric
//! to 1 and gives its own routes to `255.255.255.255` and `224.0.0.0/4` route
//! metric 0 ([`ensure_route`]: Windows has already made both, at 256, and
//! taking "already exists" for success once left the room tied with any
//! network pinned to metric 1). A [`Keeper`] re-applies all of it while the
//! room is up, adds a route for each other network's own broadcast, and checks
//! Windows' own choice of interface. `tests/lan_wintun.rs` proves a limited
//! broadcast leaves through it.

use std::io;
use std::net::Ipv4Addr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_OBJECT_ALREADY_EXISTS,
    HANDLE, HMODULE, NO_ERROR, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    CreateIpForwardEntry2, CreateUnicastIpAddressEntry, DeleteIpForwardEntry2, GetBestRoute2,
    GetIpForwardEntry2, GetIpInterfaceEntry, InitializeIpForwardEntry, InitializeIpInterfaceEntry,
    InitializeUnicastIpAddressEntry, SetIpForwardEntry2, SetIpInterfaceEntry, MIB_IPFORWARD_ROW2,
    MIB_IPINTERFACE_ROW, MIB_UNICASTIPADDRESS_ROW,
};
use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows_sys::Win32::Networking::WinSock::{
    IpDadStatePreferred, AF_INET, MIB_IPPROTO_NETMGMT, SOCKADDR_INET,
};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE,
};

use super::{AdapterConfig, AdapterSetup};
use crate::lan::LAN_MTU;
use crate::lan_pump::PacketDevice;
use crate::lan_relay::{self, RelayToken, RemoteDevice};

/// Wintun's ring size: 4 MiB, between its 128 KiB minimum and 64 MiB maximum.
const RING_CAPACITY: u32 = 0x40_0000;
/// Packets read off the adapter and not yet taken by the pump.
const READ_QUEUE: usize = 1024;
/// The environment variable a test uses to say where `wintun.dll` is.
pub const WINTUN_DLL_ENV: &str = "GAME_BRIDGE_WINTUN_DLL";

type Handle = *mut core::ffi::c_void;

// The ten functions of `wintun.h` this file uses, with its signatures.
type CreateAdapterFn = unsafe extern "system" fn(*const u16, *const u16, *const GUID) -> Handle;
type CloseAdapterFn = unsafe extern "system" fn(Handle);
type GetAdapterLuidFn = unsafe extern "system" fn(Handle, *mut NET_LUID_LH);
type StartSessionFn = unsafe extern "system" fn(Handle, u32) -> Handle;
type EndSessionFn = unsafe extern "system" fn(Handle);
type GetReadWaitEventFn = unsafe extern "system" fn(Handle) -> HANDLE;
type ReceivePacketFn = unsafe extern "system" fn(Handle, *mut u32) -> *mut u8;
type ReleaseReceivePacketFn = unsafe extern "system" fn(Handle, *const u8);
type AllocateSendPacketFn = unsafe extern "system" fn(Handle, u32) -> *mut u8;
type SendPacketFn = unsafe extern "system" fn(Handle, *const u8);
type DeleteDriverFn = unsafe extern "system" fn() -> i32;

struct WintunApi {
    _library: HMODULE,
    create_adapter: CreateAdapterFn,
    close_adapter: CloseAdapterFn,
    get_adapter_luid: GetAdapterLuidFn,
    start_session: StartSessionFn,
    end_session: EndSessionFn,
    get_read_wait_event: GetReadWaitEventFn,
    receive_packet: ReceivePacketFn,
    release_receive_packet: ReleaseReceivePacketFn,
    allocate_send_packet: AllocateSendPacketFn,
    send_packet: SendPacketFn,
    delete_driver: DeleteDriverFn,
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> io::Error {
    // SAFETY: reads this thread's last-error value.
    let code = unsafe { GetLastError() };
    io::Error::other(format!("{what}: {}", io::Error::from_raw_os_error(code as i32)))
}

impl WintunApi {
    fn load(dll: &Path) -> io::Result<Self> {
        if !dll.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is not an absolute path; wintun.dll is never loaded by search order",
                    dll.display()
                ),
            ));
        }
        let path = wide(dll.as_os_str());
        // SAFETY: a nul-terminated wide path; the flags restrict dependency
        // resolution to the DLL's own directory and System32.
        let library = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        };
        if library.is_null() {
            return Err(last_error(&format!("loading {}", dll.display())));
        }
        macro_rules! func {
            ($name:literal, $ty:ty) => {{
                // SAFETY: a nul-terminated ASCII name, looked up in the module
                // just loaded; the signature is the one `wintun.h` declares.
                let f = unsafe { GetProcAddress(library, concat!($name, "\0").as_ptr()) }
                    .ok_or_else(|| last_error(concat!("wintun.dll has no ", $name)))?;
                unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(f) }
            }};
        }
        Ok(Self {
            _library: library,
            create_adapter: func!("WintunCreateAdapter", CreateAdapterFn),
            close_adapter: func!("WintunCloseAdapter", CloseAdapterFn),
            get_adapter_luid: func!("WintunGetAdapterLUID", GetAdapterLuidFn),
            start_session: func!("WintunStartSession", StartSessionFn),
            end_session: func!("WintunEndSession", EndSessionFn),
            get_read_wait_event: func!("WintunGetReadWaitEvent", GetReadWaitEventFn),
            receive_packet: func!("WintunReceivePacket", ReceivePacketFn),
            release_receive_packet: func!("WintunReleaseReceivePacket", ReleaseReceivePacketFn),
            allocate_send_packet: func!("WintunAllocateSendPacket", AllocateSendPacketFn),
            send_packet: func!("WintunSendPacket", SendPacketFn),
            delete_driver: func!("WintunDeleteDriver", DeleteDriverFn),
        })
    }
}

/// Where `wintun.dll` is: the test's environment variable, else beside this
/// executable — which is where a release puts it.
pub fn default_wintun_dll() -> io::Result<PathBuf> {
    if let Some(p) = std::env::var_os(WINTUN_DLL_ENV) {
        return Ok(PathBuf::from(p));
    }
    Ok(std::env::current_exe()?.with_file_name("wintun.dll"))
}

/// Handles the reader thread and the device share. Wintun's receive, release,
/// allocate and send are documented thread-safe.
struct Shared {
    api: WintunApi,
    adapter: Handle,
    session: Handle,
    quit: HANDLE,
}

// SAFETY: the handles are Wintun's and Windows' own, used only through calls
// `wintun.h` documents as thread-safe, and freed once, in `Drop`, after the
// reader thread has been joined.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

/// A Wintun adapter, configured and in session. Needs an administrator.
pub struct WintunDevice {
    shared: Arc<Shared>,
    packets: tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    /// Keeps the adapter preferred while it is up ([`Keeper`]).
    keeper: Option<std::thread::JoinHandle<()>>,
}

impl WintunDevice {
    /// Create the adapter `config` names, configure it and start a session.
    pub fn create(dll: &Path, config: &AdapterConfig) -> io::Result<Self> {
        config.validate()?;
        let api = WintunApi::load(dll)?;
        let name = wide(std::ffi::OsStr::new(&config.name));
        let tunnel_type = wide(std::ffi::OsStr::new("GamingPlatformPrns"));
        // The same GUID for the same name every room, so Windows sees one
        // adapter and not "gbl0 2", "gbl0 3"… (`adapter_guid`).
        let (data1, data2, data3, data4) = super::adapter_guid(&config.name);
        let guid = GUID { data1, data2, data3, data4 };
        // SAFETY: nul-terminated wide strings and a GUID that outlive the call.
        let adapter =
            unsafe { (api.create_adapter)(name.as_ptr(), tunnel_type.as_ptr(), &guid) };
        if adapter.is_null() {
            return Err(last_error(&format!(
                "creating Wintun adapter {} (needs an administrator)",
                config.name
            )));
        }
        // SAFETY: NET_LUID_LH is plain data, filled by the call.
        let mut luid: NET_LUID_LH = unsafe { std::mem::zeroed() };
        unsafe { (api.get_adapter_luid)(adapter, &mut luid) };
        if let Err(e) = configure(luid, config) {
            unsafe { (api.close_adapter)(adapter) };
            return Err(e);
        }
        // SAFETY: a live adapter handle and a capacity inside Wintun's bounds.
        let session = unsafe { (api.start_session)(adapter, RING_CAPACITY) };
        if session.is_null() {
            let e = last_error("starting a Wintun session");
            unsafe { (api.close_adapter)(adapter) };
            return Err(e);
        }
        // SAFETY: an unnamed manual-reset event, initially unset.
        let quit = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if quit.is_null() {
            let e = last_error("creating an event");
            unsafe {
                (api.end_session)(session);
                (api.close_adapter)(adapter);
            }
            return Err(e);
        }

        let shared = Arc::new(Shared { api, adapter, session, quit });
        let (tx, rx) = mpsc::channel(READ_QUEUE);
        let reader_shared = shared.clone();
        let reader = std::thread::Builder::new()
            .name("wintun-reader".into())
            .spawn(move || read_loop(&reader_shared, &tx))?;
        let mut device =
            Self { shared, packets: tokio::sync::Mutex::new(rx), reader: Some(reader), keeper: None };
        // The session is up, so Windows now sees the adapter connected: the
        // keeper's first pass, at once, is the configuration that counts.
        let keeper_shared = device.shared.clone();
        device.keeper = Some(
            std::thread::Builder::new()
                .name("wintun-keeper".into())
                .spawn(move || keep_loop(Keeper::new(luid), &keeper_shared))?,
        );
        Ok(device)
    }
}

/// Pull packets off the ring and hand them to the pump, sleeping on Wintun's
/// read event when the ring is empty and waking for `quit`.
fn read_loop(shared: &Shared, tx: &mpsc::Sender<Vec<u8>>) {
    let api = &shared.api;
    // SAFETY: a live session; the event belongs to it.
    let readable = unsafe { (api.get_read_wait_event)(shared.session) };
    loop {
        let mut size = 0u32;
        // SAFETY: a live session; `size` receives the packet's length.
        let packet = unsafe { (api.receive_packet)(shared.session, &mut size) };
        if !packet.is_null() {
            // SAFETY: Wintun guarantees `size` readable bytes at `packet`,
            // until it is released just below.
            let bytes = unsafe { std::slice::from_raw_parts(packet, size as usize) }.to_vec();
            unsafe { (api.release_receive_packet)(shared.session, packet) };
            // A full queue drops the packet, the way a full NIC ring does.
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(bytes) {
                return;
            }
            continue;
        }
        // SAFETY: reads this thread's last-error value.
        if unsafe { GetLastError() } != ERROR_NO_MORE_ITEMS {
            return; // The adapter is going away, or its ring is corrupt.
        }
        let handles = [readable, shared.quit];
        // SAFETY: two live handles.
        let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
        if woke != WAIT_OBJECT_0 {
            return; // `quit`, or a failed wait.
        }
    }
}

impl Drop for WintunDevice {
    fn drop(&mut self) {
        // SAFETY: the event is ours; setting it wakes the reader to exit.
        unsafe { SetEvent(self.shared.quit) };
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(keeper) = self.keeper.take() {
            let _ = keeper.join();
        }
        let s = &self.shared;
        // SAFETY: the reader has exited, and these are freed exactly once.
        unsafe {
            (s.api.end_session)(s.session);
            (s.api.close_adapter)(s.adapter);
            CloseHandle(s.quit);
        }
    }
}

impl PacketDevice for WintunDevice {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let packet = self.packets.lock().await.recv().await.ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "the Wintun adapter stopped")
        })?;
        let n = packet.len().min(buf.len());
        buf[..n].copy_from_slice(&packet[..n]);
        Ok(n)
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        let s = &self.shared;
        let len = u32::try_from(packet.len()).map_err(|_| io::ErrorKind::InvalidInput)?;
        // SAFETY: a live session; on success Wintun hands back `len` writable
        // bytes, which are filled and then given back with send.
        unsafe {
            let slot = (s.api.allocate_send_packet)(s.session, len);
            if slot.is_null() {
                // A full ring drops the packet, like a full NIC; anything else
                // is the adapter going away, which the reader reports.
                return Ok(());
            }
            std::ptr::copy_nonoverlapping(packet.as_ptr(), slot, packet.len());
            (s.api.send_packet)(s.session, slot);
        }
        Ok(())
    }
}

fn check(result: u32, what: &str) -> io::Result<()> {
    if result == NO_ERROR {
        Ok(())
    } else {
        Err(io::Error::other(format!("{what}: {}", io::Error::from_raw_os_error(result as i32))))
    }
}

fn sockaddr_inet(addr: Ipv4Addr) -> SOCKADDR_INET {
    // SAFETY: plain data; all-zero is valid, and only the IPv4 arm is set.
    let mut sa: SOCKADDR_INET = unsafe { std::mem::zeroed() };
    sa.Ipv4.sin_family = AF_INET;
    sa.Ipv4.sin_addr.S_un.S_addr = u32::from(addr).to_be();
    sa
}

/// The interface metric the room adapter is given.
const ROOM_INTERFACE_METRIC: u32 = 1;

/// The multicast range, which gets the same treatment as the limited
/// broadcast: games that search by multicast search out of the cheapest
/// route to it.
const MULTICAST: (Ipv4Addr, u8) = (Ipv4Addr::new(224, 0, 0, 0), 4);

/// The room's address, the MTU, the metric and the broadcast routes.
fn configure(luid: NET_LUID_LH, config: &AdapterConfig) -> io::Result<()> {
    // SAFETY: the row is initialized by its Initialize* call before use, and
    // passed by pointer to the call that reads it.
    unsafe {
        let mut row: MIB_UNICASTIPADDRESS_ROW = std::mem::zeroed();
        InitializeUnicastIpAddressEntry(&mut row);
        row.InterfaceLuid = luid;
        row.Address = sockaddr_inet(config.address);
        row.OnLinkPrefixLength = config.subnet.prefix_len;
        row.DadState = IpDadStatePreferred;
        let r = CreateUnicastIpAddressEntry(&row);
        if r != ERROR_OBJECT_ALREADY_EXISTS {
            check(r, "setting the room address")?;
        }
    }
    set_interface(luid)?;
    ensure_route(luid, Ipv4Addr::BROADCAST, 32)?;
    ensure_route(luid, MULTICAST.0, MULTICAST.1)?;
    Ok(())
}

/// The metric, the MTU and the host model; changes only what differs, and
/// says whether it changed anything.
fn set_interface(luid: NET_LUID_LH) -> io::Result<bool> {
    // SAFETY: the row is initialized by its Initialize* call, filled by Get,
    // and passed by pointer to Set.
    unsafe {
        let mut iface: MIB_IPINTERFACE_ROW = std::mem::zeroed();
        InitializeIpInterfaceEntry(&mut iface);
        iface.Family = AF_INET;
        iface.InterfaceLuid = luid;
        check(GetIpInterfaceEntry(&mut iface), "reading the adapter's settings")?;
        let right = !iface.UseAutomaticMetric
            && iface.Metric == ROOM_INTERFACE_METRIC
            && iface.NlMtu == LAN_MTU as u32
            && iface.WeakHostReceive
            && iface.WeakHostSend;
        if right {
            return Ok(false);
        }
        // The cheapest interface is where a limited broadcast goes.
        iface.UseAutomaticMetric = false;
        iface.Metric = ROOM_INTERFACE_METRIC;
        iface.NlMtu = LAN_MTU as u32;
        // A game bound to the Wi-Fi's or the Ethernet's address is reached
        // by the pump translating the room address to it (`lan_rebind.rs`).
        // Windows is strong-host by default: it drops a packet for an address
        // the receiving adapter does not hold, and will not send one from such
        // an address. Weak host on this adapter alone lets both through; the
        // pump drops any room packet not addressed to this member, so the
        // room cannot use it to reach another address.
        iface.WeakHostReceive = true;
        iface.WeakHostSend = true;
        // SetIpInterfaceEntry refuses an IPv4 row whose SitePrefixLength is
        // not 0 (ERROR_INVALID_PARAMETER), and Get fills it in.
        iface.SitePrefixLength = 0;
        check(SetIpInterfaceEntry(&mut iface), "setting the adapter's metric, MTU and host model")?;
    }
    Ok(true)
}

fn route_row(luid: NET_LUID_LH, prefix: Ipv4Addr, len: u8) -> MIB_IPFORWARD_ROW2 {
    // SAFETY: plain data, initialized by InitializeIpForwardEntry.
    let mut route: MIB_IPFORWARD_ROW2 = unsafe { std::mem::zeroed() };
    unsafe { InitializeIpForwardEntry(&mut route) };
    route.InterfaceLuid = luid;
    route.DestinationPrefix.Prefix = sockaddr_inet(prefix);
    route.DestinationPrefix.PrefixLength = len;
    route.NextHop = sockaddr_inet(Ipv4Addr::UNSPECIFIED);
    route.Metric = 0;
    route.Protocol = MIB_IPPROTO_NETMGMT;
    route
}

/// An on-link route to `prefix/len` on this adapter **with route metric 0**.
///
/// Windows gives every interface its own `255.255.255.255/32` and
/// `224.0.0.0/4` the moment it has an address, at route metric 256 — so the
/// create below found one already there, and this used to take that for
/// success. The room then competed at 256 + its interface metric, which only
/// ties a network pinned to metric 1 by hand (Hamachi's, Radmin's, ZeroTier's
/// instructions all say to), and the tie went to the other network: "a
/// broadcast to 255.255.255.255 did not go through the room's adapter". At
/// metric 0 the room's cost is 1 against at least 256 for anything else.
fn ensure_route(luid: NET_LUID_LH, prefix: Ipv4Addr, len: u8) -> io::Result<()> {
    let what = format!("the route to {prefix}/{len}");
    let wanted = route_row(luid, prefix, len);
    // SAFETY: a fully initialized row, read by each call.
    unsafe {
        let r = CreateIpForwardEntry2(&wanted);
        if r == NO_ERROR {
            return Ok(());
        }
        if r != ERROR_OBJECT_ALREADY_EXISTS {
            return check(r, &format!("adding {what}"));
        }
        let mut there = route_row(luid, prefix, len);
        check(GetIpForwardEntry2(&mut there), &format!("reading {what}"))?;
        if there.Metric == 0 {
            return Ok(());
        }
        there.Metric = 0;
        if SetIpForwardEntry2(&there) == NO_ERROR {
            return Ok(());
        }
        // A route the system made may refuse a change; replace it instead.
        let r = DeleteIpForwardEntry2(&there);
        if r != NO_ERROR && r != ERROR_NOT_FOUND {
            return check(r, &format!("replacing {what}"));
        }
        check(CreateIpForwardEntry2(&wanted), &format!("adding {what}"))
    }
}

fn delete_route(luid: NET_LUID_LH, prefix: Ipv4Addr, len: u8) {
    let row = route_row(luid, prefix, len);
    // SAFETY: a fully initialized row, read by the call.
    unsafe { DeleteIpForwardEntry2(&row) };
}

/// The interface Windows would send `to` out of right now.
fn best_interface(to: Ipv4Addr) -> Option<u64> {
    let dst = sockaddr_inet(to);
    // SAFETY: plain data, filled by the call; the luid and source are
    // optional and passed as null.
    unsafe {
        let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        let mut src: SOCKADDR_INET = std::mem::zeroed();
        let r = GetBestRoute2(std::ptr::null(), 0, std::ptr::null(), &dst, 0, &mut row, &mut src);
        (r == NO_ERROR).then_some(row.InterfaceLuid.Value)
    }
}

/// How often the keeper puts the adapter right again.
const KEEP_EVERY_MS: u32 = 3000;

/// What keeps the room adapter preferred for as long as it is up.
///
/// Configuring once is not enough. The adapter is configured before its
/// session starts, and only then does Windows see it connected; Network
/// Location Awareness, a VPN client or a "network reset" may each put a
/// metric back; and the Wi-Fi joining another network changes which directed
/// broadcast a game computes. So the keeper re-applies all of it every few
/// seconds — each step a read that changes nothing when nothing changed — and
/// checks with Windows' own route choice that a limited broadcast really
/// leaves through the room.
struct Keeper {
    luid: NET_LUID_LH,
    /// Directed-broadcast routes this keeper added, so it takes away exactly
    /// those when a network goes.
    lan_routes: Vec<Ipv4Addr>,
    /// The last thing said, so a lasting fault is said once.
    said: Option<String>,
}

impl Keeper {
    fn new(luid: NET_LUID_LH) -> Self {
        Self { luid, lan_routes: Vec::new(), said: None }
    }

    fn keep(&mut self) {
        let mut problems = Vec::new();
        if let Err(e) = set_interface(self.luid) {
            problems.push(e.to_string());
        }
        for (prefix, len) in [(Ipv4Addr::BROADCAST, 32), MULTICAST] {
            if let Err(e) = ensure_route(self.luid, prefix, len) {
                problems.push(e.to_string());
            }
        }
        self.sync_lan_routes(&mut problems);
        // SAFETY: reading a plain union member of a copy.
        let ours = unsafe { self.luid.Value };
        match best_interface(Ipv4Addr::BROADCAST) {
            Some(luid) if luid != ours => problems.push(format!(
                "Windows still sends 255.255.255.255 out of another interface (LUID {luid:#x}): \
                 a route to it was pinned by hand below the room's"
            )),
            _ => {}
        }
        let now = (!problems.is_empty()).then(|| problems.join("; "));
        if now != self.said {
            if let Some(p) = &now {
                eprintln!("lan-helper: the room adapter is not preferred: {p}");
            }
            self.said = now;
        }
    }

    /// A host route on the room adapter to the directed broadcast of every
    /// other network this machine is on, so that a game which computes its
    /// broadcast from the Wi-Fi's address — `192.168.1.255` — reaches the room
    /// (whose pump makes it the room's broadcast) instead of the Wi-Fi. Like
    /// the limited broadcast's, these take that one address from the real
    /// network while the room is up; the pump passes on only the game's.
    fn sync_lan_routes(&mut self, problems: &mut Vec<String>) {
        let mut wanted: Vec<Ipv4Addr> = Vec::new();
        for n in crate::lan_rebind::local_networks() {
            if let Some(b) = n.broadcast() {
                if !wanted.contains(&b) {
                    wanted.push(b);
                }
            }
        }
        for b in &wanted {
            match ensure_route(self.luid, *b, 32) {
                Ok(()) => {
                    if !self.lan_routes.contains(b) {
                        self.lan_routes.push(*b);
                    }
                }
                Err(e) => problems.push(e.to_string()),
            }
        }
        let luid = self.luid;
        self.lan_routes.retain(|b| {
            let keep = wanted.contains(b);
            if !keep {
                delete_route(luid, *b, 32);
            }
            keep
        });
    }
}

/// Run `keeper` until `quit` is set.
fn keep_loop(mut keeper: Keeper, quit: &Shared) {
    loop {
        keeper.keep();
        // SAFETY: a live event handle, owned by `Shared`.
        let woke = unsafe { WaitForSingleObject(quit.quit, KEEP_EVERY_MS) };
        if woke != WAIT_TIMEOUT {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// The helper, and the launcher's side of it
// ---------------------------------------------------------------------------

/// `lan-helper serve`: create the adapter, connect out to the launcher that
/// started this, and relay until it goes away. The adapter is closed on
/// return, however the launcher ended.
///
/// `remove_driver` is portable mode: with the adapter closed, uninstall the
/// Wintun driver too, so the machine is as it was. Wintun refuses while any
/// adapter — another program's included — still exists, which is right: then
/// the driver is not only this room's to remove.
pub async fn serve(
    config: &AdapterConfig,
    launcher: std::net::SocketAddr,
    token: &RelayToken,
    dll: &Path,
    remove_driver: bool,
) -> io::Result<()> {
    let result = async {
        let device = WintunDevice::create(dll, config)?;
        let stream = lan_relay::connect_to_launcher(launcher, token).await?;
        lan_relay::relay(&device, stream).await
    }
    .await;
    if remove_driver {
        if let Err(e) = delete_driver(dll) {
            eprintln!("lan-helper: the Wintun driver was left installed: {e}");
        }
    }
    result
}

/// Uninstall the Wintun driver. Fails while any Wintun adapter exists.
pub fn delete_driver(dll: &Path) -> io::Result<()> {
    let api = WintunApi::load(dll)?;
    // SAFETY: takes no arguments; documented in wintun.h.
    if unsafe { (api.delete_driver)() } == 0 {
        return Err(last_error("removing the Wintun driver"));
    }
    Ok(())
}

/// The adapter as the launcher holds it: directly (already an administrator)
/// or through the helper's relay.
pub enum WindowsAdapter {
    /// With weak-host send on for the room's lifetime (`lan_rebind.rs`).
    Direct(WintunDevice, crate::lan_rebind::WeakHostSend),
    Relayed(RemoteDevice),
}

impl PacketDevice for WindowsAdapter {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Direct(d, _) => d.recv(buf).await,
            Self::Relayed(d) => d.recv(buf).await,
        }
    }

    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        match self {
            Self::Direct(d, _) => d.send(packet).await,
            Self::Relayed(d) => d.send(packet).await,
        }
    }
}

/// Whether this process already runs as an administrator.
pub fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // SAFETY: queries this process's own token; the handle is closed after.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

/// Start the helper with administrator rights: directly when this process
/// already has them, else through Windows' own elevation prompt — the one
/// moment a person is asked, and the only elevated code is the helper.
fn start_helper(helper: &Path, args: &[String]) -> io::Result<()> {
    if is_elevated() {
        let mut child = std::process::Command::new(helper).args(args).spawn()?;
        // Reaped on its own thread: the helper lives for the whole room.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        return Ok(());
    }
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let verb = wide(std::ffi::OsStr::new("runas"));
    let file = wide(helper.as_os_str());
    // Every argument is a name, an address, a number or hex: none needs quoting.
    let params = wide(std::ffi::OsStr::new(&args.join(" ")));
    // SAFETY: plain data, filled below; the strings outlive the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: a fully initialized SHELLEXECUTEINFOW.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(last_error(
            "starting lan-helper as an administrator (was the prompt declined?)",
        ));
    }
    Ok(())
}

/// Run the helper once with administrator rights, through Windows' own
/// prompt, and wait for it: the launcher's "Fix" and "Unblock". `in_place` is
/// Linux's distinction; here the helper always runs where it is. Its output is
/// not visible through the prompt, so success is its exit code.
pub fn run_helper_elevated(helper: &Path, args: &[String], _in_place: bool) -> io::Result<String> {
    if is_elevated() {
        let out = std::process::Command::new(helper).args(args).output()?;
        return if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        };
    }
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows_sys::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
    let verb = wide(std::ffi::OsStr::new("runas"));
    let file = wide(helper.as_os_str());
    // Every argument is a word or hex: none needs quoting.
    let params = wide(std::ffi::OsStr::new(&args.join(" ")));
    // SAFETY: plain data, filled below; the strings outlive the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOASYNC | SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: a fully initialized SHELLEXECUTEINFOW.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the administrator prompt was declined",
        ));
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other("the helper started without a process handle"));
    }
    let mut code = 1u32;
    // SAFETY: a process handle ShellExecuteExW opened for us, closed after.
    unsafe {
        WaitForSingleObject(info.hProcess, INFINITE);
        GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
    }
    if code == 0 {
        Ok(String::new())
    } else {
        Err(io::Error::other(format!("lan-helper {} failed (exit {code})", args.join(" "))))
    }
}

pub(super) async fn open(
    setup: &AdapterSetup,
    config: &AdapterConfig,
) -> anyhow::Result<WindowsAdapter> {
    match setup {
        AdapterSetup::InProcess => {
            let dll = default_wintun_dll()?;
            let config = config.clone();
            let device =
                tokio::task::spawn_blocking(move || WintunDevice::create(&dll, &config)).await??;
            let weak_host =
                tokio::task::spawn_blocking(crate::lan_rebind::WeakHostSend::on).await?;
            Ok(WindowsAdapter::Direct(device, weak_host))
        }
        AdapterSetup::Helper { path, portable } => {
            let mut extra = Vec::new();
            if let Some(dll) = std::env::var_os(WINTUN_DLL_ENV) {
                extra.push("--wintun".to_string());
                extra.push(dll.to_string_lossy().into_owned());
            }
            // Portable: the Wintun driver and this room's firewall rule both go
            // when the room does.
            if *portable {
                extra.push("--portable".to_string());
            }
            let helper = path.clone();
            let remote =
                super::relayed(config, extra, move |args| start_helper(&helper, &args)).await?;
            Ok(WindowsAdapter::Relayed(remote))
        }
    }
}
