Amberol
=======

![Application icon](./data/icons/hicolor/scalable/apps/io.bassi.Amberol.svg)

A small and simple sound and music player that is well integrated with GNOME.

Amberol aspires to be as small, unintrusive, and simple as possible. It does
not manage your music collection; it does not let you manage playlists, smart
or otherwise; it does not let you edit the metadata for your songs; it does
not show you lyrics for your songs, or the Wikipedia page for your bands.

Amberol plays music, and nothing else.

This is **Sweet Amberol**, a fork maintained by
[haleff](https://github.com/ha1eff) with a Lollypop-style album grid, a stable
session restore, low memory use and tools for editing cover artwork.

![Random albums](https://github.com/user-attachments/assets/fbad4827-304c-47ea-a8ba-d3c8498a6b32)
![Playlist and now playing](https://github.com/user-attachments/assets/491e2510-e366-41de-90a4-c92c76a442e1)

Building a .deb package
-----------------------

Install the build dependencies once:

    sudo apt install cargo meson ninja-build gettext libxml2-utils pkg-config \
      libgtk-4-dev libadwaita-1-dev \
      libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
      libgstreamer-plugins-bad1.0-dev libgstreamer-bad-audio1.0-dev \
      blueprint-compiler desktop-file-utils

Set up the build directory and run the packaging script:

    meson setup build --prefix=/usr/local
    ./packaging/build-deb.sh local

The script compiles the project, stages a minimal install tree and writes the
package to `dist/amberol_2026.2+local_amd64.deb`. Install it with:

    sudo apt install ./dist/amberol_2026.2+local_amd64.deb

On later builds only the packaging script is needed. If you edit a `.blp` file
or the stylesheet and the interface does not update, force a resource rebuild:

    rm -f build/src/amberol.gresource
    meson compile -C build

Getting in touch
----------------

If you have questions about Amberol, you can join the [`#amberol:gnome.org`](https://matrix.to/#/#amberol:gnome.org)
channel on Matrix, or use the [GNOME Discourse instance](https://discourse.gnome.org/c/applications/7).

Contributing
------------

Please, see the [contribution guide](./CONTRIBUTING.md) if you wish to report
and issue, fix a bug, or implement a new feature.

How to obtain debugging information
-----------------------------------

Run Amberol from your terminal using:

    RUST_BACKTRACE=1 RUST_LOG=amberol=debug amberol

to obtain a full debug log.

Translations
------------

Amberol is translated on the [GNOME translation platform](https://l10n.gnome.org/module/amberol).

You should contact the coordinator of [the localization team for your language](https://l10n.gnome.org/teams/)
if you have questions.

For more information, please see the [GNOME Translation Project Welcome page](https://welcome.gnome.org/team/translation/).

Code of conduct
---------------

Amberol follows the GNOME project [Code of Conduct](./code-of-conduct.md). All
communications in project spaces, such as the issue tracker or
[Discourse](https://discourse.gnome.org) are expected to follow it.

Why is it called "Amberol"?
---------------------------

The name comes from the the [Blue Amberol
Records](https://en.wikipedia.org/wiki/Blue_Amberol_Records), a type of cylinder
records made of (blue) nitrocellulose, capable of playback durations of around
four minutes, just about the length of the average song since 1990.

Copyright and licensing
-----------------------

Copyright 2022  Emmanuele Bassi

Amberol is released under the terms of the GNU General Public License, either
version 3.0 or, at your option, any later version.
