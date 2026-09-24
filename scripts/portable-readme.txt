Mesh Game Servers — portable

Everything this launcher keeps lives in this folder: settings, identities,
remembered servers, and the web view's storage and caches. Nothing is written
anywhere else on the machine.

LAN rooms install nothing either. On Linux your password is asked when a room
starts (polkit's pkexec runs a copy of lan-helper that is deleted as soon as
it runs); on Windows, administrator rights are asked, and the Wintun driver is
removed again when the room ends. The room's network adapter disappears with
the launcher, even if the launcher crashes or is killed.

Delete this folder to forget everything. Delete the whole directory and
nothing of Mesh Game Servers is left on this machine.
