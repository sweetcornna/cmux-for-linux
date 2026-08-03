"""Nautilus context menu entries for cmux.

Adds "New cmux window here" and "New cmux workspace here" to the menu for
selected folders and for the background of the folder being viewed.

Both entries shell out to cmux-open-here, which every file-manager integration
this package ships uses, so the behaviour is identical across them.

Requires nautilus-python (Debian/Ubuntu: python3-nautilus, Fedora:
nautilus-python). The file is inert when that is not installed.
"""

import shutil

import gi

gi.require_version("Nautilus", "4.1")

from gi.repository import Gio, GObject, Nautilus  # noqa: E402

HELPER = shutil.which("cmux-open-here") or "/usr/bin/cmux-open-here"

# Opening dozens of terminals from one click is never intended, so a large
# multi-selection suppresses the entries instead.
MAX_FOLDERS = 10


def folder_paths(files):
    paths = []
    for file_info in files:
        if not file_info.is_directory():
            return []
        path = file_info.get_location().get_path()
        # Non-local locations (sftp://, trash://, …) have no local path and
        # cannot be a working directory.
        if path is None:
            return []
        if path not in paths:
            paths.append(path)
    return paths if len(paths) <= MAX_FOLDERS else []


def run_helper(_menu_item, mode, paths):
    for path in paths:
        Gio.Subprocess.new([HELPER, mode, path], Gio.SubprocessFlags.NONE)


def build_items(prefix, files):
    paths = folder_paths(files)
    if not paths:
        return []

    window = Nautilus.MenuItem(
        name=f"{prefix}::window",
        label="New cmux window here",
        tip="Open a terminal running cmux in this folder",
        icon="utilities-terminal-symbolic",
    )
    window.connect("activate", run_helper, "window", paths)

    workspace = Nautilus.MenuItem(
        name=f"{prefix}::workspace",
        label="New cmux workspace here",
        tip="Create a cmux workspace for this folder in the running session",
        icon="tab-new-symbolic",
    )
    workspace.connect("activate", run_helper, "workspace", paths)

    return [window, workspace]


class CmuxMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    def get_file_items(self, files):
        return build_items("CmuxNautilus::selection", files)

    def get_background_items(self, current_folder):
        return build_items("CmuxNautilus::background", [current_folder])
