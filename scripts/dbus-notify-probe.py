#!/usr/bin/env python3
"""A throwaway org.freedesktop.Notifications service for tests.

notify-rust talks to the session bus, so a headless test needs a service that
answers `Notify`, `GetCapabilities` and `GetServerInformation`. This one writes
every call as one JSON object per line to `$OVVC_NOTIFY_LOG` and touches
`$OVVC_NOTIFY_READY` once the well-known name is owned.

Run it through scripts/e2e-dbus-notifications.sh, which starts dbus-run-session
and points the daemon's notifier test at the log.
"""

import json
import os
import warnings

warnings.filterwarnings("ignore", category=DeprecationWarning)

from gi.repository import Gio, GLib

XML = """
<node>
  <interface name="org.freedesktop.Notifications">
    <method name="Notify">
      <arg type="s" direction="in"/>
      <arg type="u" direction="in"/>
      <arg type="s" direction="in"/>
      <arg type="s" direction="in"/>
      <arg type="s" direction="in"/>
      <arg type="as" direction="in"/>
      <arg type="a{sv}" direction="in"/>
      <arg type="i" direction="in"/>
      <arg type="u" direction="out"/>
    </method>
    <method name="CloseNotification">
      <arg type="u" direction="in"/>
    </method>
    <method name="GetCapabilities">
      <arg type="as" direction="out"/>
    </method>
    <method name="GetServerInformation">
      <arg type="s" direction="out"/>
      <arg type="s" direction="out"/>
      <arg type="s" direction="out"/>
      <arg type="s" direction="out"/>
    </method>
    <signal name="NotificationClosed">
      <arg type="u"/>
      <arg type="u"/>
    </signal>
    <signal name="ActionInvoked">
      <arg type="u"/>
      <arg type="s"/>
    </signal>
  </interface>
  <interface name="org.freedesktop.DBus.Properties">
    <method name="Get">
      <arg type="s" direction="in"/>
      <arg type="s" direction="in"/>
      <arg type="v" direction="out"/>
    </method>
    <method name="GetAll">
      <arg type="s" direction="in"/>
      <arg type="a{sv}" direction="out"/>
    </method>
  </interface>
</node>
"""

LOG = os.environ["OVVC_NOTIFY_LOG"]
READY = os.environ["OVVC_NOTIFY_READY"]
_sequence = 0


def record(kind, **fields):
    fields["method"] = kind
    with open(LOG, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(fields) + "\n")
        handle.flush()


def on_call(connection, sender, path, interface, method, parameters, invocation):
    global _sequence
    if method == "Notify":
        app, replaces, icon, summary, body, actions, hints, expire = parameters.unpack()
        _sequence += 1
        record(
            "Notify",
            app=app,
            replaces=replaces,
            icon=icon,
            summary=summary,
            body=body,
            actions=actions,
            expire=expire,
        )
        invocation.return_value(GLib.Variant("(u)", (_sequence,)))
    elif method == "GetCapabilities":
        invocation.return_value(GLib.Variant("(as)", (["body", "actions", "icon-static"],)))
    elif method == "GetServerInformation":
        invocation.return_value(
            GLib.Variant("(ssss)", ("ovvc-notification-probe", "ovvc", "1.0", "1.2"))
        )
    elif method == "CloseNotification":
        invocation.return_value(None)
    elif method == "Get":
        invocation.return_value(GLib.Variant("(v)", (GLib.Variant("s", ""),)))
    elif method == "GetAll":
        invocation.return_value(GLib.Variant("(a{sv})", ({},)))
    else:
        invocation.return_value(None)


def main():
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    node = Gio.DBusNodeInfo.new_for_xml(XML)
    for interface in node.interfaces:
        connection.register_object(
            "/org/freedesktop/Notifications", interface, on_call, None, None
        )
    reply = connection.call_sync(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        GLib.Variant("(su)", ("org.freedesktop.Notifications", 0)),
        None,
        Gio.DBusCallFlags.NONE,
        1000,
        None,
    )
    owned = reply.unpack()[0]
    record("ready", owned=owned)
    if owned != 1:
        raise SystemExit(
            f"dbus-notify-probe: org.freedesktop.Notifications is already owned (reply {owned})"
        )
    with open(READY, "w", encoding="utf-8") as handle:
        handle.write("ready\n")
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
