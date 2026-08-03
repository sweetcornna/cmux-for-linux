# RPM spec for the cmux-gtk frontend package.
#
# Kept separate from cmux.spec rather than parameterised: the two packages have
# different file lists, different dependencies and different %check assertions,
# and a single spec juggling both with conditionals would be harder to read
# than two short ones.
#
# Like cmux.spec this is a binary repack; see the note there.

%global debug_package %{nil}
%global __os_install_post %{nil}

Name:           cmux-gtk
Version:        %{cmux_version}
Release:        1%{?dist}
Summary:        GTK4 frontend for the cmux terminal multiplexer

License:        GPL-3.0-or-later
URL:            https://github.com/sweetcornna/cmux-for-linux
Source0:        %{cmux_stage_tar}

BuildRequires:  tar
Requires:       gtk4
# The frontend attaches to a session the cmux package provides, but it is only
# a recommendation: the session may live on another machine.
Recommends:     cmux

%description
A GTK4 window onto a running cmux session: it lists the session's workspaces,
renders a terminal from the server's styled render stream, and sends input
back.

It contains no terminal emulator. The cmux server is the only VT
implementation; this package draws the styled runs it sends.

%prep
%setup -q -c -T
tar xf %{SOURCE0} -C .

%build
# Nothing to compile: see the note at the top of this spec.

%install
mkdir -p %{buildroot}
cp -a usr %{buildroot}/

%check
test -x %{buildroot}%{_bindir}/cmux-gtk

%files
%license %{_datadir}/licenses/cmux-gtk/LICENSE
%doc %{_datadir}/doc/cmux-gtk/README.md
%{_datadir}/doc/cmux-gtk/copyright
%{_bindir}/cmux-gtk
%{_datadir}/applications/cmux-gtk.desktop

%changelog
* Sun Aug 02 2026 cmux for linux <travon_evenietyku@sanfranmail.com>
- Initial packaging of the GTK4 frontend.
