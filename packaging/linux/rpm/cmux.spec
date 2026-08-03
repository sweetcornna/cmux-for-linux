# RPM spec for cmux for linux.
#
# This is a binary-repack spec: the cmux binaries and data files are built
# beforehand by packaging/linux/build-binaries.sh and laid out by
# packaging/linux/stage-tree.sh, and this spec turns that tree into an RPM.
# Building the Rust workspace inside rpmbuild would require vendoring every
# crate and a Zig 0.16 toolchain in the buildroot, which no target distro
# currently ships.

%global debug_package %{nil}
%global __os_install_post %{nil}

Name:           cmux
Version:        %{cmux_version}
Release:        1%{?dist}
Summary:        Terminal multiplexer with TUI and GTK4 frontends for AI coding agents

License:        GPL-3.0-or-later
URL:            https://github.com/sweetcornna/cmux-for-linux
Source0:        %{cmux_stage_tar}

BuildRequires:  tar
Requires:       glibc
Requires:       gtk4
Obsoletes:      cmux-gtk
Provides:       cmux-gtk = %{version}-%{release}
# The Nautilus context-menu entries need nautilus-python; the extension file
# is inert without it, so this stays a weak dependency.
Suggests:       nautilus-python

%description
cmux keeps a tree of machines, sessions, workspaces, screens, panes, tabs,
terminals and browsers, and exposes them through a noun-first CLI and a
terminal UI. Terminal emulation is handled by libghostty-vt.

This package is built from the cmux for linux fork and ships the cmux-tui
multiplexer, the cmux-relay transport primitive, the cmux-gtk GTK4 frontend,
a man page, shell completions and desktop entries for both frontends.

%prep
%setup -q -c -T
tar xf %{SOURCE0} -C .

%build
# Nothing to compile: see the note at the top of this spec.

%install
mkdir -p %{buildroot}
cp -a usr %{buildroot}/

%check
test -x %{buildroot}%{_bindir}/cmux-tui
test -x %{buildroot}%{_bindir}/cmux-relay
test -x %{buildroot}%{_bindir}/cmux-gtk

%files
%license %{_datadir}/licenses/cmux/LICENSE
%doc %{_datadir}/doc/cmux/README.md
%doc %{_datadir}/doc/cmux/GUI.md
%doc %{_datadir}/doc/cmux/THIRD_PARTY_LICENSES.md
%{_datadir}/doc/cmux/copyright
%{_bindir}/cmux
%{_bindir}/cmux-tui
%{_bindir}/cmux-relay
%{_bindir}/cmux-gtk
%{_bindir}/cmux-open-here
# File-manager context menus. The parent directories are co-owned so the
# package is installable whether or not the matching file manager is present.
%dir %{_datadir}/nautilus-python
%dir %{_datadir}/nautilus-python/extensions
%{_datadir}/nautilus-python/extensions/cmux.py
%dir %{_datadir}/kio/servicemenus
%{_datadir}/kio/servicemenus/cmux-open-here.desktop
%dir %{_datadir}/file-manager
%dir %{_datadir}/file-manager/actions
%{_datadir}/file-manager/actions/cmux-open-here.desktop
%dir %{_datadir}/nemo
%dir %{_datadir}/nemo/actions
%{_datadir}/nemo/actions/cmux-window.nemo_action
%{_datadir}/nemo/actions/cmux-workspace.nemo_action
%{_datadir}/applications/cmux.desktop
%{_datadir}/applications/cmux-gtk.desktop
%{_datadir}/icons/hicolor/*/apps/cmux.png
%{_mandir}/man1/cmux.1*
%{_datadir}/bash-completion/completions/cmux
%{_datadir}/zsh/site-functions/_cmux
%{_datadir}/fish/vendor_completions.d/cmux.fish

%changelog
* Mon Aug 03 2026 cmux for linux <travon_evenietyku@sanfranmail.com>
- Merge the GTK4 frontend into the full cmux package.

* Sun Aug 02 2026 cmux for linux <travon_evenietyku@sanfranmail.com>
- Initial native Linux packaging for the cmux TUI and relay.
