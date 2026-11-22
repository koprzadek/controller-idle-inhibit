#!/usr/bin/env python3
"""Minimal fake Wayland compositor used to integration-test
controller-idle-inhibit without a real desktop running.

Implements just enough of the protocol for the daemon's handshake:
wl_display, wl_registry, wl_callback, wl_compositor, wl_region, wl_shm,
wl_shm_pool, wl_buffer, wl_surface, zwlr_layer_shell_v1,
zwlr_layer_surface_v1, zwp_idle_inhibit_manager_v1.

Usage:
    python3 mock_compositor.py SOCKET_PATH LOG_PATH [--closed-test]

Writes one marker per step to LOG_PATH, e.g.:

    GLOBALS-SENT
    BIND wl_compositor v1
    CREATE-POOL fd=yes size=4
    CREATE-BUFFER 1x1 stride=4 fmt=0
    GET-LAYER-SURFACE surface=9 output=10 layer=0 namespace=controller-idle-inhibit
    NULL-OUTPUT            (only if a client passes output=0 — river would crash)
    INITIAL-COMMIT surface=9
    CONFIGURE-SENT serial=1 1x1
    ACK serial=1
    ATTACH surface=9 buffer=8
    MAPPED surface=9
    CREATE-INHIBITOR inhibitor=12 surface=9
    DESTROY-INHIBITOR inhibitor=12
    CLOSED-SENT layer_surface=11
    SURFACE-DESTROYED surface=9
    CLIENT-DISCONNECT

With --closed-test, right after the first MAPPED the mock simulates the
output going away: it sends layer_surface.closed, removes the old wl_output
global and advertises a brand-new one, letting the daemon prove that it
rebuilds the surface on the new output.
"""

import array
import os
import socket
import struct
import sys

assert sys.byteorder == "little", "wayland wire is native-endian; only LE tested"

SOL_SOCKET, SCM_RIGHTS = 1, 1

HEADER = struct.Struct("<IHH")
U32 = struct.Struct("<I")
I32 = struct.Struct("<i")

def pack_string(s: str) -> bytes:
    raw = s.encode() + b"\0"
    pad = (-len(raw)) % 4
    return U32.pack(len(raw)) + raw + b"\0" * pad

GLOBALS = [
    (1, "wl_compositor", 5),
    (2, "wl_shm", 1),
    (3, "wl_output", 4),
    (4, "zwlr_layer_shell_v1", 5),
    (5, "zwp_idle_inhibit_manager_v1", 1),
]

class Mock:
    def __init__(self, sock_path: str, log_path: str, closed_test: bool):
        self.sock_path = sock_path
        self.log = open(log_path, "a", buffering=1)
        self.closed_test = closed_test
        self.buf = b""
        self.types = {}          # object id -> interface name
        self.surfaces = {}       # surface id -> state dict
        self.layer_of_surface = {}
        self.inhibitors = {}     # inhibitor id -> surface id
        self.next_serial = 1
        self.closed_sent = set()
        self.closed_fired = False
        self.next_global_name = 6
        self.current_registry = None

    def mark(self, msg: str):
        self.log.write(msg + "\n")

    # ---------- event sending ----------

    def send_event(self, obj: int, opcode: int, payload: bytes = b""):
        self.conn.sendall(HEADER.pack(obj, opcode, 8 + len(payload)) + payload)

    def send_globals(self, registry_id: int):
        for name, iface, version in GLOBALS:
            payload = U32.pack(name) + pack_string(iface) + U32.pack(version)
            self.send_event(registry_id, 0, payload)
        self.mark("GLOBALS-SENT")

    # ---------- parsing ----------

    @staticmethod
    def parse_string(data: bytes, off: int):
        (length,) = U32.unpack_from(data, off)
        off += 4
        raw = data[off : off + length - 1]
        off += length
        off += (-off) % 4
        return raw.decode(), off

    def dispatch(self, obj_id: int, opcode: int, payload: bytes):
        t = self.types.get(obj_id, "wl_display" if obj_id == 1 else "?")
        if obj_id == 1:
            t = "wl_display"
        key = (t, opcode)

        if key == ("wl_display", 0):  # sync(new_id)
            (cb,) = U32.unpack_from(payload, 0)
            self.types[cb] = "wl_callback"
            self.send_event(cb, 0, U32.pack(0))  # done
            return
        if key == ("wl_display", 1):  # get_registry(new_id)
            (reg,) = U32.unpack_from(payload, 0)
            self.types[reg] = "wl_registry"
            self.current_registry = reg
            self.send_globals(reg)
            return

        if key == ("wl_registry", 0):  # bind(name, interface, version, new_id)
            (name,) = U32.unpack_from(payload, 0)
            iface, off = self.parse_string(payload, 4)
            (version,) = U32.unpack_from(payload, off)
            (new_id,) = U32.unpack_from(payload, off + 4)
            self.types[new_id] = iface
            self.mark(f"BIND {iface} v{version} as {new_id} (global {name})")
            return

        if key == ("wl_compositor", 0):  # create_surface
            (sid,) = U32.unpack_from(payload, 0)
            self.types[sid] = "wl_surface"
            self.surfaces[sid] = {
                "configured": False, "acked": None,
                "pending_buffer": None, "mapped": False,
            }
            return
        if key == ("wl_compositor", 1):  # create_region
            (rid,) = U32.unpack_from(payload, 0)
            self.types[rid] = "wl_region"
            return
        if t == "wl_region":
            return  # destroy/add/subtract: state not tracked

        if key == ("wl_shm", 0):  # create_pool(new_id, fd, size) — fd via cmsg
            (pid,) = U32.unpack_from(payload, 0)
            (size,) = I32.unpack_from(payload, 4)
            self.types[pid] = "wl_shm_pool"
            self.mark(f"CREATE-POOL id={pid} size={size} fd={len(self.last_fds)}")
            return
        if key == ("wl_shm_pool", 0):  # create_buffer(new_id, off, w, h, stride, fmt)
            vals = struct.unpack_from("<IiiiiI", payload, 0)
            bid, off_, w, h, stride, fmt = vals
            self.types[bid] = "wl_buffer"
            self.mark(f"CREATE-BUFFER id={bid} {w}x{h} stride={stride} fmt={fmt}")
            return
        if key == ("wl_buffer", 0):
            return

        if key == ("wl_surface", 5):  # set_input_region(region)
            (rid,) = U32.unpack_from(payload, 0)
            self.mark(f"SET-INPUT-REGION surface={obj_id} region={rid}")
            return
        if key == ("wl_surface", 0):  # destroy
            self.mark(f"SURFACE-DESTROYED surface={obj_id}")
            self.surfaces.pop(obj_id, None)
            # wlroots kills inhibitors with their surface:
            for iid, surf in list(self.inhibitors.items()):
                if surf == obj_id:
                    del self.inhibitors[iid]
                    self.mark(f"DESTROY-INHIBITOR inhibitor={iid} (surface destroyed)")
            return
        if key == ("wl_surface", 1):  # attach(buffer, x, y)
            (bid,) = U32.unpack_from(payload, 0)
            self.surfaces.setdefault(obj_id, {})["pending_buffer"] = bid or None
            if bid:
                self.mark(f"ATTACH surface={obj_id} buffer={bid}")
            return
        if key == ("wl_surface", 6):  # commit
            self.handle_commit(obj_id)
            return

        if key == ("zwlr_layer_shell_v1", 0):  # get_layer_surface
            lid, sid = struct.unpack_from("<II", payload, 0)
            (out,) = U32.unpack_from(payload, 8)
            off = 12
            layer, = U32.unpack_from(payload, off); off += 4
            ns, off = self.parse_string(payload, off)
            self.types[lid] = "zwlr_layer_surface_v1"
            self.layer_of_surface[sid] = lid
            if out == 0:
                self.mark("NULL-OUTPUT")
            self.mark(
                f"GET-LAYER-SURFACE surface={sid} layer_surface={lid} "
                f"output={out} layer={layer} namespace={ns}"
            )
            return

        if t == "zwlr_layer_surface_v1":
            if opcode == 0:  # set_size
                w, h = struct.unpack_from("<II", payload, 0)
                self.mark(f"SET-SIZE {w}x{h}")
            elif opcode == 1:  # set_anchor
                (a,) = U32.unpack_from(payload, 0)
                self.mark(f"SET-ANCHOR {a}")
            elif opcode == 2:  # set_exclusive_zone
                (z,) = I32.unpack_from(payload, 0)
                self.mark(f"SET-EXCLUSIVE-ZONE {z}")
            elif opcode == 6:  # ack_configure(serial)
                (serial,) = U32.unpack_from(payload, 0)
                self.mark(f"ACK serial={serial}")
                for st in self.surfaces.values():
                    if st.get("acked") is None:
                        st["acked"] = serial
            elif opcode == 7:  # destroy
                self.mark(f"LAYER-SURFACE-DESTROYED {obj_id}")
            return

        if key == ("zwp_idle_inhibit_manager_v1", 0):
            self.mark("INHIBIT-MANAGER-DESTROYED")
            return
        if key == ("zwp_idle_inhibit_manager_v1", 1):  # create_inhibitor(id, surface)
            iid, sid = struct.unpack_from("<II", payload, 0)
            self.types[iid] = "zwp_idle_inhibitor_v1"
            self.inhibitors[iid] = sid
            self.mark(f"CREATE-INHIBITOR inhibitor={iid} surface={sid}")
            return
        if key == ("zwp_idle_inhibitor_v1", 0):  # destroy
            self.inhibitors.pop(obj_id, None)
            self.mark(f"DESTROY-INHIBITOR inhibitor={obj_id}")
            return

        self.mark(f"UNKNOWN obj={obj_id}({t}) op={opcode} len={len(payload)}")

    def handle_commit(self, sid: int):
        st = self.surfaces.get(sid)
        if st is None:
            return
        lid = self.layer_of_surface.get(sid)
        if lid is None:
            return
        if not st["configured"]:
            st["configured"] = True
            serial = self.next_serial
            self.next_serial += 1
            self.mark(f"INITIAL-COMMIT surface={sid}")
            self.mark(f"CONFIGURE-SENT serial={serial} 1x1")
            self.send_event(lid, 0, struct.pack("<III", serial, 1, 1))
            return
        if st["pending_buffer"] is not None and st["acked"] is not None:
            if not st["mapped"]:
                st["mapped"] = True
                self.mark(f"MAPPED surface={sid}")
                if self.closed_test and not self.closed_fired:
                    self.closed_fired = True
                    self.simulate_output_removed(lid)
                return
        # empty commit (unmap) — ignore

    def simulate_output_removed(self, lid: int):
        # closed event on the layer surface
        self.send_event(lid, 1)
        self.mark("CLOSED-SENT")
        # remove old wl_output global (name 3) and add a fresh one
        self.send_event(self.current_registry, 1, U32.pack(3))
        self.mark("GLOBAL-REMOVED 3")
        new_name = self.next_global_name
        self.next_global_name += 1
        payload = U32.pack(new_name) + pack_string("wl_output") + U32.pack(4)
        self.send_event(self.current_registry, 0, payload)
        self.mark(f"GLOBAL-ADDED {new_name} wl_output")

    # ---------- main loop ----------

    def run(self):
        try:
            os.unlink(self.sock_path)
        except FileNotFoundError:
            pass
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(self.sock_path)
        server.listen(1)
        self.mark("LISTENING")
        conn, _ = server.accept()
        self.conn = conn
        self.last_fds = []

        while True:
            try:
                data, ancdata, _flags, _addr = conn.recvmsg(65536)
            except (ConnectionResetError, OSError):
                break
            if not data:
                break

            self.last_fds = []
            for level, ctype, cdata in ancdata:
                if level == SOL_SOCKET and ctype == SCM_RIGHTS:
                    fds = array.array("i")
                    fds.frombytes(cdata[: len(cdata) - (len(cdata) % fds.itemsize)])
                    self.last_fds.extend(fds)
                    for fd in fds:
                        os.close(fd)  # we do not need the memfd

            self.buf += data
            while len(self.buf) >= 8:
                obj_id, opcode, size = HEADER.unpack_from(self.buf, 0)
                if size < 8 or len(self.buf) < size:
                    break
                payload = self.buf[8:size]
                self.buf = self.buf[size:]
                try:
                    self.dispatch(obj_id, opcode, payload)
                except Exception as exc:  # never crash mid-test
                    self.mark(f"MOCK-ERROR {exc!r}")

        self.mark("CLIENT-DISCONNECT")


def main():
    sock_path, log_path = sys.argv[1], sys.argv[2]
    closed_test = "--closed-test" in sys.argv[3:]
    Mock(sock_path, log_path, closed_test).run()


if __name__ == "__main__":
    main()
