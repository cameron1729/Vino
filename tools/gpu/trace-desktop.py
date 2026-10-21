#!/usr/bin/env python3
"""Read-only X11 movement trace: geometry and pointer coordinates, never screen pixels/titles.

Listens for root-child ConfigureNotify and XI2 raw pointer events. Window-manager
frames are root children, so their movement is visible even with compositing on.
Event receipt times are not render/presentation times. Pointer queries are only
made in response to raw input, with their duration logged to expose perturbation.
"""
import argparse
import ctypes as c
import json
import select
import signal
import time


class Configure(c.Structure):
    _fields_ = [("type", c.c_int), ("serial", c.c_ulong), ("send_event", c.c_int),
                ("display", c.c_void_p), ("event", c.c_ulong), ("window", c.c_ulong),
                ("x", c.c_int), ("y", c.c_int), ("width", c.c_int), ("height", c.c_int),
                ("border_width", c.c_int), ("above", c.c_ulong), ("override_redirect", c.c_int)]


class Generic(c.Structure):
    _fields_ = [("type", c.c_int), ("serial", c.c_ulong), ("send_event", c.c_int),
                ("display", c.c_void_p), ("extension", c.c_int), ("evtype", c.c_int),
                ("cookie", c.c_uint), ("data", c.c_void_p)]


class Event(c.Union):
    _fields_ = [("type", c.c_int), ("configure", Configure), ("generic", Generic),
                ("pad", c.c_long * 24)]


class EventMask(c.Structure):
    _fields_ = [("deviceid", c.c_int), ("mask_len", c.c_int), ("mask", c.POINTER(c.c_ubyte))]


def function(lib, name, result, *args):
    func = getattr(lib, name)
    func.restype = result
    func.argtypes = list(args)
    return func


def emit(event, **fields):
    print(json.dumps(dict(event=event, monotonic_ns=time.monotonic_ns(), **fields)), flush=True)


class Desktop:
    def __init__(self):
        self.x = c.CDLL("libX11.so.6")
        self.xi = c.CDLL("libXi.so.6")
        ptr, integer, window = c.c_void_p, c.c_int, c.c_ulong
        pi, pw = c.POINTER(integer), c.POINTER(window)
        function(self.x, "XOpenDisplay", ptr, c.c_char_p)
        function(self.x, "XCloseDisplay", integer, ptr)
        function(self.x, "XDefaultRootWindow", window, ptr)
        function(self.x, "XConnectionNumber", integer, ptr)
        function(self.x, "XQueryExtension", integer, ptr, c.c_char_p, pi, pi, pi)
        function(self.x, "XSelectInput", integer, ptr, window, c.c_long)
        function(self.x, "XSync", integer, ptr, integer)
        function(self.x, "XPending", integer, ptr)
        function(self.x, "XNextEvent", integer, ptr, c.POINTER(Event))
        function(self.x, "XQueryPointer", integer, ptr, window, pw, pw, pi, pi, pi, pi,
                 c.POINTER(c.c_uint))
        function(self.xi, "XIQueryVersion", integer, ptr, pi, pi)
        function(self.xi, "XISelectEvents", integer, ptr, window, c.POINTER(EventMask), integer)
        emit("initializing", step="open_display")
        self.display = self.x.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError("cannot open the desktop X11 display")
        try:
            self.root = self.x.XDefaultRootWindow(self.display)
            self.fd = self.x.XConnectionNumber(self.display)
            opcode, first_event, first_error = integer(), integer(), integer()
            emit("initializing", step="query_input")
            if not self.x.XQueryExtension(self.display, b"XInputExtension", c.byref(opcode),
                                         c.byref(first_event), c.byref(first_error)):
                raise RuntimeError("XInput extension is unavailable")
            self.opcode = opcode.value
            # XI 2.1+ reports raw events even while another client has grabbed the pointer.
            major, minor = integer(2), integer(2)
            if self.xi.XIQueryVersion(self.display, c.byref(major), c.byref(minor)) != 0:
                raise RuntimeError("XInput 2.2 is unavailable")
            emit("initializing", step="subscribe")
            self.x.XSelectInput(self.display, self.root, 1 << 19)  # SubstructureNotifyMask
            bits = (c.c_ubyte * 3)()
            for kind in (15, 16, 17):  # RawButtonPress, RawButtonRelease, RawMotion
                bits[kind >> 3] |= 1 << (kind & 7)
            mask = EventMask(1, len(bits), bits)  # XIAllMasterDevices
            if self.xi.XISelectEvents(self.display, self.root, c.byref(mask), 1) != 0:
                raise RuntimeError("cannot subscribe to raw pointer events")
            self.x.XSync(self.display, False)
        except BaseException:
            self.close()
            raise

    def close(self):
        if self.display:
            self.x.XCloseDisplay(self.display)
            self.display = None

    def pointer(self, raw_events):
        root, child = c.c_ulong(), c.c_ulong()
        x, y, wx, wy = c.c_int(), c.c_int(), c.c_int(), c.c_int()
        buttons = c.c_uint()
        started = time.monotonic_ns()
        ok = self.x.XQueryPointer(self.display, self.root, c.byref(root), c.byref(child),
                                 c.byref(x), c.byref(y), c.byref(wx), c.byref(wy), c.byref(buttons))
        emit("pointer", x=x.value, y=y.value, button1=bool(buttons.value & (1 << 8)),
             same_screen=bool(ok), raw_events=raw_events,
             query_ms=(time.monotonic_ns() - started) / 1e6)

    def trace(self, duration):
        stopped = False

        def stop(_signum, _frame):
            nonlocal stopped
            stopped = True

        signal.signal(signal.SIGTERM, stop)
        signal.signal(signal.SIGINT, stop)
        started, cpu = time.monotonic(), time.process_time()
        positions = {}
        raw_total = configured = 0
        emit("start", duration_s=duration, root=self.root)
        self.pointer(0)
        while not stopped and time.monotonic() - started < duration:
            if not self.x.XPending(self.display):
                select.select([self.fd], [], [], min(0.1, max(0, duration - (time.monotonic() - started))))
            raw = 0
            # Bound draining so an input flood cannot prevent shutdown.
            for _ in range(512):
                if not self.x.XPending(self.display):
                    break
                event = Event()
                self.x.XNextEvent(self.display, c.byref(event))
                if event.type == 22:  # ConfigureNotify
                    cfg = event.configure
                    geometry = (cfg.x, cfg.y, cfg.width, cfg.height)
                    if cfg.event == self.root and positions.get(cfg.window) != geometry:
                        positions[cfg.window] = geometry
                        emit("configure", window=cfg.window, x=cfg.x, y=cfg.y,
                             width=cfg.width, height=cfg.height, synthetic=bool(cfg.send_event))
                        configured += 1
                elif (event.type == 35 and event.generic.extension == self.opcode
                      and event.generic.evtype in (15, 16, 17)):
                    # No cookie payload is fetched, so no allocated event data needs freeing.
                    raw += 1
            if raw:
                raw_total += raw
                self.pointer(raw)
        emit("exit", elapsed_s=time.monotonic() - started, process_cpu_seconds=time.process_time() - cpu,
             raw_events=raw_total, geometry_events=configured)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--duration", type=float, default=60)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if not 0 < args.duration <= 950:
        parser.error("duration must be greater than zero and at most 950 seconds")
    desktop = Desktop()
    try:
        if args.check:
            emit("check", ok=True)
        else:
            desktop.trace(args.duration)
    finally:
        desktop.close()


if __name__ == "__main__":
    main()
