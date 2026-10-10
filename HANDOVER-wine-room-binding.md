# Handover: LAN room joins hang when a member runs the game under Wine/Proton

Found 2026-10-10 against release **v0.2.29** (portable build), game pack
`nfs-underground-2`. Linux host (CachyOS, game via Faugus → umu → Proton-CachyOS)
and a Windows member. Both firewalls off. Neither direction could join.

## Symptom

- Linux hosts → the Windows member sees the race in the game's LAN list, but
  joining hangs / is refused.
- Windows hosts → the Linux member cannot join either.
- Turning off both OS firewalls changes nothing.

## Evidence (Linux host, room up, race hosted)

Room adapter was up and passing traffic:

```
gbl0  UNKNOWN  198.19.45.221/16
198.19.0.0/16 dev gbl0 src 198.19.45.221
255.255.255.255 dev gbl0 scope link
default via 192.168.32.1 dev wlan0 src 192.168.32.203 metric 600
```

What the game is bound to (`ss -tulpn`):

```
udp UNCONN 0.0.0.0:9999          SPEED2.EXE / wineserver   <- discovery, wildcard
tcp LISTEN 192.168.32.203:9900   wineserver                <- Wi-Fi address only
```

Probe:

```
198.19.45.221:9900   Connection refused   (room address)
192.168.32.203:9900  OPEN                 (wlan0 address)
```

## Root cause

Wine binds the game's sockets to what it believes is the machine's own
address. Wine's local-address list (`gethostbyname(own hostname)` /
adapter ordering) puts the adapter that carries the **default route** first —
wlan0 — never `gbl0`, which only has a connected /16 route.

So:

1. **Linux hosts:** UDP 9999 is bound to `0.0.0.0`, so the discovery broadcast
   crosses the room fine and the race shows up. The Windows client answers the
   broadcast's source address (`198.19.45.221`) and connects to TCP 9900 there.
   Nothing listens on the room address → refused / hang.
2. **Windows hosts:** the same thing in reverse. The Linux member's callback
   listeners (TCP 3282/3284/3285, UDP 3658/3659) are most likely bound to the
   Wi-Fi address, and outgoing sockets bound to it leave `gbl0` with source
   `192.168.32.203`, which is not a room address. (Not verified on the Windows
   side — check `netstat -ano | findstr "9900 3282 3658"` there when hosting.)

The room's filter (`lan_filter.rs`) is **not** at fault here — nothing it
logged explains this, and the firewall is irrelevant. v0.2.29's room check
already *detects* it (`lan_check.rs` ~line 378: "a game run through Wine or with
a fixed address set can do that") but does not fix it.

## Workaround (verified to be correct in shape; not yet run — needs root)

`scripts/wine-room-fix.sh` — two nftables NAT rules on the Linux member,
applied after the room is up:

- `prerouting`: `iifname gbl0 ip daddr <room ip> dnat to <lan ip>` — room
  traffic reaches the socket bound to the LAN address.
- `postrouting`: `oifname gbl0 ip saddr <lan ip> snat to <room ip>` — the game's
  packets leave the room with the room address as source.

```
sudo scripts/wine-room-fix.sh       # apply
sudo scripts/wine-room-fix.sh off   # remove
```

Not persistent; gone on reboot. The pump sees post-NAT packets, so the filter
still sees room addresses only.

**To confirm:** apply on the Linux member, run the room check from the
Windows member — TCP 9900 must flip from Closed to listening — then race in
both directions.

## Proposed product fixes

1. **Recommended: rewrite addresses in the pump (`lan_pump.rs`).** Every room
   packet already passes through `pump()` in user space, so the pump can do a
   static 1:1 translation between the room address and the local address the
   game actually bound — no extra privilege, nothing left behind, and the same
   shape on every platform.
   - **`down` (room → device):** after `f.inbound()` admits a packet and after
     the `lan_check::is_probe` branch, if `dst == room.own_address()` and a
     rebind is active, set `dst` to the bound LAN address before
     `device.send()`. Linux accepts it on `gbl0`: it is a weak-host system and
     delivers a packet for any local address on any interface (`rp_filter`
     checks only the source, which is a room address routed via `gbl0`).
   - **`up` (device → room):** right after `device.recv()`, before
     `f.outbound()`, if `src` is the bound LAN address, set `src` to
     `room.own_address()`. The kernel already routes these into `gbl0` because
     the destination is in `198.19.0.0/16`; only the source is wrong. Doing it
     before the filter means the flow table, `probe_log` and `connect_watch`
     all see room addresses only.
   - **Checksums:** recompute the IPv4 header checksum and the TCP/UDP checksum
     (the pseudo-header includes both addresses). An incremental RFC 1624
     update is enough. A UDP checksum of 0 means "none" and stays 0. A non-first
     fragment has no transport header, so only its IP checksum changes.
   - **ICMP errors** quote the original header inside them. Rewrite the
     embedded addresses too, or a refused connect will look like a hang
     instead of failing fast.
   - **Security:** the translation runs *behind* `LanFilter` on the way in, so
     only ports the pack declares (plus the player's extras) reach the LAN
     address. It does not widen what a member can reach.
   - **When to turn it on:** the room check already finds "nothing listening on
     TCP <port> at the room address". Have it also probe the game's declared
     TCP ports on this machine's other local addresses. If exactly one address
     answers, offer "The game is listening on 192.168.32.203 instead of the
     room's address (Wine does this) — Fix", or just turn it on. Store the bound
     address on `LanSession`, the same way `extra_ports()` is shared with the
     pump, and re-read it when the LAN address changes (DHCP).
   - **Windows:** the same code would work there, but Windows is a strong-host
     system and drops a packet for an address that is not on the receiving
     adapter. The elevated helper would have to enable weak-host receive on
     the room adapter only (`MIB_IPINTERFACE_ROW.WeakHostReceive = TRUE` with
     `SetIpInterfaceEntry`, next to the metric in `lan_adapter/windows.rs`
     `configure`). Only do this if Windows is ever seen binding to the wrong
     address (see item 5).
   - **Caveat:** this rewrites only the IP headers. If a game puts its own IP
     *inside* its payload and peers use it, this does not help. The nfsu2relay
     captures say NFSU2 answers the packet's source address instead, so it
     should be fine. Run `scripts/wine-room-fix.sh` (same translation, done by
     the kernel) before building this to confirm a race actually starts.
   - **Tests:** extend `tests/lan_pack_ports.rs`, which already runs through
     real adapters in network namespaces: a stand-in listener bound to the
     namespace's "LAN" address only must be reachable at the room address, and
     its replies must leave with the room address as source.

2. **Alternative, Linux only: NAT rules in `lan-helper`.** It already runs with `CAP_NET_ADMIN`
   for the room's lifetime. Install the two NAT rules (scoped to `gbl0`) when the
   room starts and drop the table when the helper exits. Should be done via
   netlink/nftables directly, not by shelling out to `nft` — a helper granted
   `cap_net_admin` by file capability does not pass it to children (see
   `lan_adapter/linux.rs` header). The LAN address changes with DHCP, so either
   re-resolve it or SNAT any non-room source leaving `gbl0`, and DNAT to the
   address the listener is actually on (from the room check's probe, or
   `fib daddr type local` on all local addresses).
   - Alternative to investigate: give `gbl0` an indirect (gateway) route with a
     metric below the default route's, so Wine ranks it first. Cheaper, but it
     depends on Wine's ordering logic; the NAT approach does not.
3. **Room check wording:** when it sees a listener on the game's port on a
   *non-room* local address, say so outright ("the game is listening on
   192.168.32.203, not the room's address — Wine does this") and offer the fix,
   instead of the generic "nothing is listening".
4. **Ignore Steam's LAN discovery in the refused log.** Proton runs a
   `steam.exe` that broadcasts on **UDP 27036**. It showed up as a "missing
   port", got Allowed for `nfs-underground-2` (`lan_extra_ports` in
   `launcher.json`), and does nothing for the game. Add 27036 (and probably
   27031–27036, Steam In-Home Streaming) to `OS_CHATTER` in `lan_filter.rs`, or
   a separate "background chatter" list, so it is never suggested.
5. **Windows-host direction:** confirm on the Windows machine which address
   NFSU2 binds when hosting. The adapter metric (1) set in
   `lan_adapter/windows.rs` should make `gbl0` first there, but that has not been
   observed with this game.
