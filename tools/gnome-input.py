"""Injects pointer and keyboard input into a headless GNOME Shell session
through Mutter's RemoteDesktop D-Bus API, for GUI tests (see docs/PLAN.md,
"Building and running"). Keeps one D-Bus connection open: Mutter ends the
session when its creator disconnects.

Usage: gnome-input.py STEP...  with steps like
  move:X,Y   down / up (left button)   drag:X1,Y1,X2,Y2   sleep:SECONDS
  rclick:X,Y (right button)   scroll:X,Y,STEPS (wheel; positive is down)
  key:NAME   keydown:NAME   keyup:NAME   code:EVDEV_KEYCODE
Coordinates are monitor pixels.
"""
import sys
import time

from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)


def call(dest, path, iface, method, args=None, reply=None):
    return bus.call_sync(dest, path, iface, method, args,
                         GLib.VariantType(reply) if reply else None,
                         Gio.DBusCallFlags.NONE, -1, None)


RD = "org.gnome.Mutter.RemoteDesktop"
SC = "org.gnome.Mutter.ScreenCast"
session = call(RD, "/org/gnome/Mutter/RemoteDesktop", RD, "CreateSession",
               reply="(o)").unpack()[0]
session_id = call(RD, session, "org.freedesktop.DBus.Properties", "Get",
                  GLib.Variant("(ss)", (RD + ".Session", "SessionId")),
                  reply="(v)").unpack()[0]
cast = call(SC, "/org/gnome/Mutter/ScreenCast", SC, "CreateSession",
            GLib.Variant("(a{sv})", ({"remote-desktop-session-id":
                                      GLib.Variant("s", session_id)},)),
            reply="(o)").unpack()[0]
stream = call(SC, cast, SC + ".Session", "RecordMonitor",
              GLib.Variant("(sa{sv})", ("", {})), reply="(o)").unpack()[0]
call(RD, session, RD + ".Session", "Start")
time.sleep(0.5)

BTN_LEFT = 0x110
BTN_RIGHT = 0x111


def rd(method, fmt, *args):
    call(RD, session, RD + ".Session", method, GLib.Variant(fmt, args))


def move(x, y):
    rd("NotifyPointerMotionAbsolute", "(sdd)", stream, float(x), float(y))


def button(pressed, which=BTN_LEFT):
    rd("NotifyPointerButton", "(ib)", which, pressed)


def key(sym, pressed):
    rd("NotifyKeyboardKeysym", "(ub)", sym, pressed)


KEYSYMS = {"Control_L": 0xFFE3, "Shift_L": 0xFFE1, "Delete": 0xFFFF,
           "Escape": 0xFF1B, "space": 0x20, "Up": 0xFF52, "Down": 0xFF54,
           "Left": 0xFF51, "Right": 0xFF53, "F10": 0xFFC7, "Home": 0xFF50, "End": 0xFF57,
           "BackSpace": 0xFF08, "Menu": 0xFF67}


# evdev keycodes: special keys sent as keysyms don't always arrive.
KEYCODES = {"Control_L": 29, "Shift_L": 42, "Alt_L": 56, "Delete": 111,
            "BackSpace": 14, "Escape": 1, "space": 57, "Up": 103, "Down": 108,
            "Left": 105, "Right": 106, "F10": 68, "Home": 102, "End": 107,
            "Menu": 127, "Return": 28, "Tab": 15, "F9": 67,
            "comma": 51, "period": 52}


def sym(name):
    return KEYSYMS.get(name) or ord(name)


def press(name, pressed):
    if name in KEYCODES:
        rd("NotifyKeyboardKeycode", "(ub)", KEYCODES[name], pressed)
    else:
        key(sym(name), pressed)


# The first key event of a session is dropped (keyboard focus arriving);
# spend it on a Shift tap.
press("Shift_L", True)
press("Shift_L", False)
time.sleep(0.2)

for step in sys.argv[1:]:
    name, _, arg = step.partition(":")
    if name == "move":
        move(*map(float, arg.split(",")))
    elif name == "down":
        button(True)
    elif name == "up":
        button(False)
    elif name == "rclick":
        move(*map(float, arg.split(",")))
        time.sleep(0.1)
        button(True, BTN_RIGHT)
        button(False, BTN_RIGHT)
    elif name == "scroll":
        x, y, steps = arg.split(",")
        move(float(x), float(y))
        time.sleep(0.1)
        # Discrete wheel clicks on the vertical axis (0).
        rd("NotifyPointerAxisDiscrete", "(ui)", 0, int(steps))
    elif name == "drag":
        x1, y1, x2, y2 = map(float, arg.split(","))
        move(x1, y1)
        time.sleep(0.1)
        button(True)
        for i in range(1, 21):
            move(x1 + (x2 - x1) * i / 20, y1 + (y2 - y1) * i / 20)
            time.sleep(0.02)
        button(False)
    elif name == "key":
        press(arg, True)
        press(arg, False)
    elif name == "code":
        # evdev keycode (e.g. 111 Delete, 14 BackSpace)
        rd("NotifyKeyboardKeycode", "(ub)", int(arg), True)
        rd("NotifyKeyboardKeycode", "(ub)", int(arg), False)
    elif name == "keydown":
        press(arg, True)
    elif name == "keyup":
        press(arg, False)
    elif name == "sleep":
        time.sleep(float(arg))
    time.sleep(0.15)
# The compositor may still be delivering the last events.
time.sleep(1.0)
print("input done")
