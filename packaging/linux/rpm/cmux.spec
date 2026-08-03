# RPM spec for the cmux Linux maintenance branch.
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
Summary:        Terminal multiplexer TUI for AI coding agents, backed by libghostty-vt

License:        GPL-3.0-or-later
URL:            https://github.com/sweetcornna/cmux
Source0:        %{cmux_stage_tar}

BuildRequires:  tar
Requires:       glibc

%description
cmux keeps a tree of machines, sessions, workspaces, screens, panes, tabs,
terminals and browsers, and exposes them through a noun-first CLI and a
terminal UI. Terminal emulation is handled by libghostty-vt.

This package is built from the Linux maintenance branch of cmux and ships the
cmux-tui multiplexer, the cmux-relay transport primitive, a man page, shell
completions and a desktop entry.

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

%files
%license %{_datadir}/licenses/cmux/LICENSE
%doc %{_datadir}/doc/cmux/README.md
%doc %{_datadir}/doc/cmux/THIRD_PARTY_LICENSES.md
%{_datadir}/doc/cmux/copyright
%{_bindir}/cmux
%{_bindir}/cmux-tui
%{_bindir}/cmux-relay
%{_datadir}/applications/cmux.desktop
%{_datadir}/icons/hicolor/*/apps/cmux.png
%{_mandir}/man1/cmux.1*
%{_datadir}/bash-completion/completions/cmux
%{_datadir}/zsh/site-functions/_cmux
%{_datadir}/fish/vendor_completions.d/cmux.fish

%changelog
* Sun Aug 02 2026 cmux Linux maintenance branch <travon_evenietyku@sanfranmail.com>
- Initial native Linux packaging for the cmux TUI and relay.
