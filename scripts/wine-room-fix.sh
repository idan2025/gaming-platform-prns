#!/usr/bin/env bash
# Make a Wine/Proton game that binds to the Wi-Fi/Ethernet address reachable
# through the Mesh Game Servers room adapter (gbl0).
#
#   sudo ./wine-room-fix.sh        # apply (run after the room is up)
#   sudo ./wine-room-fix.sh off    # remove
#
# Inbound:  room traffic to the room address is DNAT'ed to the LAN address the
#           game actually listens on.
# Outbound: anything leaving gbl0 with the LAN address as source is SNAT'ed to
#           the room address, so the other members (and the room's filter) see
#           the right sender.
set -euo pipefail
TABLE=mgs_wine_fix
IFACE=${IFACE:-gbl0}

nft delete table ip "$TABLE" 2>/dev/null || true
[[ "${1:-}" == off ]] && { echo "removed"; exit 0; }

ROOM=$(ip -4 -o addr show dev "$IFACE" | awk '{split($4,a,"/"); print a[1]; exit}')
LAN=$(ip -4 route get 1.1.1.1 | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')
[[ -n "$ROOM" ]] || { echo "no $IFACE address — start the room first" >&2; exit 1; }
[[ -n "$LAN" ]]  || { echo "could not find the LAN address" >&2; exit 1; }

nft -f - <<EOF
table ip $TABLE {
    chain pre {
        type nat hook prerouting priority dstnat; policy accept;
        iifname "$IFACE" ip daddr $ROOM dnat to $LAN
    }
    chain post {
        type nat hook postrouting priority srcnat; policy accept;
        oifname "$IFACE" ip saddr $LAN snat to $ROOM
    }
}
EOF
echo "room $ROOM <-> lan $LAN on $IFACE: applied (undo: sudo $0 off)"
