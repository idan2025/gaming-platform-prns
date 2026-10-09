//! Mode 3 on Windows with the firewall **on**: what a player's machine does to
//! a room, and what the room does about it (`lan_firewall.rs`).
//!
//! The first real Need for Speed: Underground 2 race in a room hung on joining:
//! a firewall dropped the join, and the player saw a game they could not
//! enter. `lan_wintun.rs` runs with the firewall off, as an honest stand-in for
//! a game that asked for its own rule — which is exactly the assumption that
//! turned out false. This one turns it on, blocking every inbound connection
//! nothing allows, and walks the whole story on a real Wintun adapter:
//!
//! 1. With no rule, a member's TCP connection to a listening program here gets
//!    no answer, and the host's pump notices (`ConnectWatch`).
//! 2. The room rule (`open`, on `198.18.0.0/15`) lets it through.
//! 3. A Block rule for the program beats that allow — what Windows makes when
//!    someone clicks "Cancel" on a firewall prompt — and `windows_facts` names
//!    it, and `unblock` lifts it.
//! 4. The real `lan-helper` opens the firewall by itself: a portable room adds
//!    its own rule and removes it when the room ends; an installed room's rule
//!    stays.
//!
//! The member has no adapter (Windows has no network namespaces), so it speaks
//! raw IPv4 into the room: a SYN, and it reads what comes back.
//!
//! Needs an administrator, `wintun.dll` and a firewall that blocks inbound by
//! default: CI's `lan-windows` job sets all three. Without the DLL it skips.

#![cfg(windows)]

use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use game_bridge::lan_adapter::{is_elevated, run_room_on_adapter, AdapterSetup, WINTUN_DLL_ENV};
use game_bridge::lan_filter::{LanPolicy, LanPort, LanProto};
use game_bridge::lan_firewall::{
    self, open, unblock, windows_facts, Lifetime, LocalFirewall, System, PORTABLE_RULE_NAME,
    RULE_NAME,
};
use game_bridge::lan_session::{LanHostArgs, LanMemberArgs, LanSession};
use game_bridge::profile::GameProfile;

mod common;

const GAME_PORT: u16 = 9900;
const BLOCK_RULE: &str = "gpp-test-block";

fn profile() -> GameProfile {
    let mut p = GameProfile::sven_coop();
    p.id = "lan-wintun-firewall-test".to_string();
    p.app_name = "lan-wintun-firewall-test".to_string();
    p.query = None;
    p
}

fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    for part in parts {
        let mut chunks = part.chunks_exact(2);
        for c in &mut chunks {
            sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        if let [last] = chunks.remainder() {
            sum += (*last as u32) << 8;
        }
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// A TCP segment with both checksums right: Windows drops a wrong one
/// silently, which would look exactly like the firewall under test.
fn tcp_packet(
    src: Ipv4Addr,
    sport: u16,
    dst: Ipv4Addr,
    dport: u16,
    flags: u8,
    seq: u32,
) -> Vec<u8> {
    let mut p = vec![0u8; 40];
    p[0] = 0x45;
    p[2..4].copy_from_slice(&40u16.to_be_bytes());
    p[8] = 64;
    p[9] = 6;
    p[12..16].copy_from_slice(&src.octets());
    p[16..20].copy_from_slice(&dst.octets());
    let ip_sum = checksum(&[&p[..20]]);
    p[10..12].copy_from_slice(&ip_sum.to_be_bytes());
    p[20..22].copy_from_slice(&sport.to_be_bytes());
    p[22..24].copy_from_slice(&dport.to_be_bytes());
    p[24..28].copy_from_slice(&seq.to_be_bytes());
    p[32] = 5 << 4; // data offset: 20 bytes
    p[33] = flags;
    p[34..36].copy_from_slice(&64240u16.to_be_bytes()); // window
    let mut pseudo = [0u8; 12];
    pseudo[..4].copy_from_slice(&src.octets());
    pseudo[4..8].copy_from_slice(&dst.octets());
    pseudo[9] = 6;
    pseudo[10..12].copy_from_slice(&20u16.to_be_bytes());
    let sum = checksum(&[&pseudo, &p[20..]]);
    p[36..38].copy_from_slice(&sum.to_be_bytes());
    p
}

const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;

/// Send SYNs from the member to the host's game port and return the flags of
/// the answer, if any came within a few seconds.
async fn connect_from_member(
    member: &LanSession,
    m_addr: Ipv4Addr,
    h_addr: Ipv4Addr,
    sport: u16,
) -> Option<u8> {
    let deadline = Instant::now() + Duration::from_secs(6);
    let mut next_send = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next_send {
            member.send(tcp_packet(m_addr, sport, h_addr, GAME_PORT, SYN, 1000)).unwrap();
            next_send = Instant::now() + Duration::from_secs(2);
        }
        let Ok(Some(p)) = tokio::time::timeout(Duration::from_millis(250), member.recv()).await
        else {
            continue;
        };
        let ihl = (p[0] & 0x0f) as usize * 4;
        if p.len() < ihl + 14 || p[9] != 6 {
            continue;
        }
        let from = Ipv4Addr::new(p[12], p[13], p[14], p[15]);
        let fport = u16::from_be_bytes([p[ihl], p[ihl + 1]]);
        let tport = u16::from_be_bytes([p[ihl + 2], p[ihl + 3]]);
        if from == h_addr && fport == GAME_PORT && tport == sport {
            let flags = p[ihl + 13];
            // Tidy: tell the host to forget the half-open connection.
            let _ = member.send(tcp_packet(m_addr, sport, h_addr, GAME_PORT, RST, 1001));
            return Some(flags);
        }
    }
    None
}

fn accepted(answer: Option<u8>) -> bool {
    answer.is_some_and(|f| f & (SYN | ACK) == SYN | ACK)
}

fn netsh(args: &[&str]) -> bool {
    std::process::Command::new("netsh")
        .args(["advfirewall", "firewall"])
        .args(args)
        .output()
        .expect("netsh runs")
        .status
        .success()
}

fn rule_exists(name: &str) -> bool {
    netsh(&["show", "rule", &format!("name={name}")])
}

fn delete_rule(name: &str) {
    let _ = netsh(&["delete", "rule", &format!("name={name}")]);
}

/// Every Wintun adapter Windows knows, hidden ones included, as
/// `name|{GUID}`. One room after another must leave exactly one, named `gbl0`:
/// a random GUID per room made Windows number them "gbl0 2", "gbl0 3"…
fn wintun_adapters() -> Vec<String> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-NetAdapter -IncludeHidden | Where-Object InterfaceDescription -like '*Wintun*' | \
             ForEach-Object { $_.Name + '|' + $_.InterfaceGuid }",
        ])
        .output()
        .expect("powershell runs");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn assert_one_gbl0(when: &str) {
    let (d1, d2, d3, d4) = game_bridge::lan_adapter::adapter_guid("gbl0");
    let guid = format!(
        "{{{d1:08X}-{d2:04X}-{d3:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    );
    let adapters = wintun_adapters();
    assert_eq!(adapters, [format!("gbl0|{guid}")], "{when}: the room adapter is one, named gbl0");
}

fn holds(addr: Ipv4Addr) -> bool {
    UdpSocket::bind((addr, 0)).is_ok()
}

async fn wait_until(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Put `host` on an adapter until the returned sender is used or dropped.
fn room_on_adapter(
    host: Arc<LanSession>,
    setup: AdapterSetup,
) -> (tokio::sync::oneshot::Sender<()>, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let policy =
        LanPolicy { ports: vec![LanPort { proto: LanProto::Tcp, port: GAME_PORT }], any: false };
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let runner =
        tokio::spawn(run_room_on_adapter(host, policy, "gbl0".to_string(), setup, async move {
            let _ = stop_rx.await;
        }));
    (stop_tx, runner)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_firewall_that_drops_the_room_is_noticed_opened_and_unblocked() {
    if std::env::var_os(WINTUN_DLL_ENV).is_none() {
        eprintln!("skipping: set {WINTUN_DLL_ENV} to wintun.dll (CI's lan-windows job does)");
        return;
    }
    assert!(is_elevated(), "the firewall test needs an administrator");
    for name in [RULE_NAME, PORTABLE_RULE_NAME, BLOCK_RULE] {
        delete_rule(name);
    }

    let dir = common::scratch_dir("lan-wintun-firewall");
    let mesh_port = common::free_tcp_port();
    let mut host_args = LanHostArgs::new(profile());
    host_args.identity = dir.join("host.identity");
    host_args.tcp = Some(format!("0.0.0.0:{mesh_port}"));
    host_args.announce_interval = 1;
    let host = Arc::new(LanSession::host(host_args).await.unwrap());
    let mut member_args = LanMemberArgs::new(profile());
    member_args.identity = dir.join("member.identity");
    member_args.tcp = Some(format!("127.0.0.1:{mesh_port}"));
    member_args.room_hash =
        Some(host.room_hash().unwrap().as_bytes().iter().map(|b| format!("{b:02x}")).collect());
    let member = LanSession::join(member_args).await.unwrap();
    wait_until("the member is seated", Duration::from_secs(60), || member.own_address().is_some())
        .await;
    let (h_addr, m_addr) = (host.own_address().unwrap(), member.own_address().unwrap());

    // The game: something listening, so an answer is a SYN-ACK.
    let _game = TcpListener::bind(("0.0.0.0", GAME_PORT)).unwrap();

    // --- 1. No rule. The adapter is this process's own, so no helper has
    // touched the firewall.
    let (stop, runner) = room_on_adapter(host.clone(), AdapterSetup::InProcess);
    wait_until("the adapter holds the host's address", Duration::from_secs(60), || holds(h_addr))
        .await;
    assert_one_gbl0("the first room");
    let answer = connect_from_member(&member, m_addr, h_addr, 40001).await;
    assert!(
        answer.is_none(),
        "a member reached a program nothing allows ({answer:?}): this test needs the firewall on and \
         blocking inbound (CI's lan-windows job sets it)"
    );
    wait_until("the host's pump notices the unanswered join", Duration::from_secs(10), || {
        host.connect_watch()
            .dropped(Instant::now())
            .iter()
            .any(|d| d.port == GAME_PORT && d.from == m_addr)
    })
    .await;

    // --- 2. The room rule lets it through.
    assert_eq!(
        open(&System, LocalFirewall::Windows, "gbl0", Lifetime::Kept).unwrap(),
        lan_firewall::Opened::Added(LocalFirewall::Windows)
    );
    assert!(rule_exists(RULE_NAME));
    let answer = connect_from_member(&member, m_addr, h_addr, 40002).await;
    assert!(accepted(answer), "the room rule did not let the join in: {answer:?}");
    let facts = windows_facts(&System).expect("PowerShell reads the firewall without trouble");
    assert!(facts.rule_present, "{facts:?}");

    // --- 3. A Block rule for the program beats the allow, is named, and is
    // lifted.
    let exe = std::env::current_exe().unwrap();
    assert!(netsh(&[
        "add",
        "rule",
        &format!("name={BLOCK_RULE}"),
        "dir=in",
        "action=block",
        &format!("program={}", exe.display()),
        "enable=yes",
    ]));
    let answer = connect_from_member(&member, m_addr, h_addr, 40003).await;
    assert!(answer.is_none(), "a Block rule did not beat the room rule: {answer:?}");
    let facts = windows_facts(&System).unwrap();
    let block = facts
        .blocks
        .iter()
        .find(|b| b.display_name == BLOCK_RULE)
        .unwrap_or_else(|| panic!("the Block rule is not named: {facts:?}"));
    assert!(block.program.eq_ignore_ascii_case(&exe.display().to_string()), "{block:?}");
    assert_eq!(unblock(&System, std::slice::from_ref(&block.name)).unwrap(), 1);
    let answer = connect_from_member(&member, m_addr, h_addr, 40004).await;
    assert!(accepted(answer), "the unblocked program is still blocked: {answer:?}");
    delete_rule(BLOCK_RULE);

    stop.send(()).unwrap();
    runner.await.unwrap().unwrap();
    wait_until("the adapter is gone", Duration::from_secs(30), || !holds(h_addr)).await;

    // --- 4. The real helper does it by itself. Portable: its own rule, gone
    // with the room.
    delete_rule(RULE_NAME);
    let helper: std::path::PathBuf = env!("CARGO_BIN_EXE_lan-helper").into();
    let (stop, runner) = room_on_adapter(
        host.clone(),
        AdapterSetup::Helper { path: helper.clone(), portable: true },
    );
    wait_until("the portable adapter comes up", Duration::from_secs(60), || holds(h_addr)).await;
    assert_one_gbl0("the second room");
    assert!(rule_exists(PORTABLE_RULE_NAME), "the portable helper opened nothing");
    assert!(!rule_exists(RULE_NAME), "a portable room left an installed launcher's rule");
    let answer = connect_from_member(&member, m_addr, h_addr, 40005).await;
    assert!(accepted(answer), "the portable helper's rule did not let the join in: {answer:?}");
    stop.send(()).unwrap();
    runner.await.unwrap().unwrap();
    wait_until("the portable adapter is gone", Duration::from_secs(30), || !holds(h_addr)).await;
    wait_until("the portable rule is gone with the room", Duration::from_secs(30), || {
        !rule_exists(PORTABLE_RULE_NAME)
    })
    .await;

    // Installed: the rule stays, so the next room needs nothing.
    let (stop, runner) =
        room_on_adapter(host.clone(), AdapterSetup::Helper { path: helper, portable: false });
    wait_until("the adapter comes up", Duration::from_secs(60), || holds(h_addr)).await;
    assert_one_gbl0("the third room");
    let answer = connect_from_member(&member, m_addr, h_addr, 40006).await;
    assert!(accepted(answer), "the installed helper's rule did not let the join in: {answer:?}");
    stop.send(()).unwrap();
    runner.await.unwrap().unwrap();
    wait_until("the adapter is gone", Duration::from_secs(30), || !holds(h_addr)).await;
    assert!(rule_exists(RULE_NAME), "an installed room's rule did not outlive the room");

    delete_rule(RULE_NAME);
}
