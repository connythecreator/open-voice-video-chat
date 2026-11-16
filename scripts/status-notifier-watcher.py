#!/usr/bin/env python3
"""A minimal org.kde.StatusNotifierWatcher for the tray integration test.

ksni refuses to register a StatusNotifierItem unless a watcher owns this name.
Desktop environments provide one; a private test bus does not. This watcher
records the item name, claims a host is registered, and answers the property
queries ksni makes while starting.

Set `OVVC_WATCHER_READY` to the file touched once the name is owned.
"""

import os
import warnings

warnings.filterwarnings("ignore", category=DeprecationWarning)

from gi.repository import Gio, GLib

XML = """
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem">
      <arg type="s" direction="in"/>
    </method>
    <method name="RegisterStatusNotifierHost">
      <arg type="s" direction="in"/>
    </method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
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

READY = os.environ["OVVC_WATCHER_READY"]
LOG = os.environ.get("OVVC_WATCHER_LOG")
items = []


def log(message):
    if not LOG:
        return
    with open(LOG, "a", encoding="utf-8") as handle:
        handle.write(message + "\n")
        handle.flush()


def properties():
    return {
        "RegisteredStatusNotifierItems": GLib.Variant("as", items),
        "IsStatusNotifierHostRegistered": GLib.Variant("b", True),
        "ProtocolVersion": GLib.Variant("i", 0),
    }


def on_call(connection, sender, path, interface, method, parameters, invocation):
    if method == "RegisterStatusNotifierItem":
        item = parameters.unpack()[0]
        if not item.startswith("/"):
            item = sender + item
        items.append(item)
        log(f"registered {item}")
        connection.emit_signal(
            None,
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
            "StatusNotifierItemRegistered",
            GLib.Variant("(s)", (item,)),
        )
        invocation.return_value(None)
    elif method == "RegisterStatusNotifierHost":
        invocation.return_value(None)
    elif method == "Get":
        _, name = parameters.unpack()
        values = properties()
        if name not in values:
            invocation.return_value(GLib.Variant("(v)", (GLib.Variant("s", ""),)))
        else:
            invocation.return_value(GLib.Variant("(v)", (values[name],)))
    elif method == "GetAll":
        invocation.return_value(GLib.Variant("(a{sv})", (properties(),)))
    else:
        invocation.return_value(None)


def main():
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    node = Gio.DBusNodeInfo.new_for_xml(XML)
    for interface in node.interfaces:
        connection.register_object(
            "/StatusNotifierWatcher", interface, on_call, None, None
        )
    reply = connection.call_sync(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        GLib.Variant("(su)", ("org.kde.StatusNotifierWatcher", 0)),
        None,
        Gio.DBusCallFlags.NONE,
        1000,
        None,
    )
    owned = reply.unpack()[0]
    if owned != 1:
        raise SystemExit(
            f"status-notifier-watcher: name already owned (reply {owned})"
        )
    log("watcher ready")
    with open(READY, "w", encoding="utf-8") as handle:
        handle.write("ready\n")
    GLib.MainLoop().run()


if __name__ == "__main__":
    main()
