# In a browser, or from another machine

edualcd, the daemon the window starts, can also serve windows that aren't on this machine: a web
page that is the same window, and an Android app. How it does that safely is
[below](#how-it-is-kept-safe).

## Serving the page

1. Run `systemctl --user enable --now edualcd`. It listens on the loopback address.
2. Run `edualcd --pair`. It prints an address that opens the page, or pairs a window or the app.
   The token is in the part of the address that is never sent to a server.

To reach edualcd from other machines, see the comments in `edualcd.service`. `edualcd --demo`
serves the page to anyone, with a script in place of Claude Code, on a demo project.

## On Android

The app lists the edualcds it's paired with, and pairs with another by the address
`edualcd --pair` prints. When it starts, it reaches the one it reached last and opens the session
it showed last there. A demo's address (`http://host:port/`) works too, and the window says its
connection isn't encrypted. "Choose another edualcd" in the settings goes back to the list. The
app keeps its pairings out of Android's backups.

Links open in Android's browser, notifications are Android's (it asks the first time the window
opens), and the composer's image button pastes the clipboard's image. On the screen's keyboard,
Enter starts a new line and the send button sends; on a keyboard with keys, Enter sends.

The app keeps its connection only while it's on screen. Android stops an app in the background;
the app reaches edualcd again when it comes back, so you hear of a session waiting for you only
then. It draws images on the window's thread, which many large ones slow down.

## How it is kept safe

A window on this machine still uses the Unix socket, with its checks. `edualcd --listen ADDRESS`
also accepts connections at `/api/line` over TLS, each one more window of the same daemon.

On its first start, edualcd makes a certificate for itself and a token, and keeps them in
`$XDG_STATE_HOME/edualcd`, a folder that is yours alone; write your own token to its file `token`
to choose it. The token is taken from that file alone: never from the command line, which every
user of the machine can read, nor from the environment, which every program edualcd runs
inherits. `edualcd --pair` prints `https://host:port/#token=…&fingerprint=…`. The token is in the
fragment, which is never sent to a server. A window trusts exactly the certificate it paired
with, by its SHA-256, whatever it names or is signed by.

The token lets whoever holds it run Claude Code as you, so it is kept like an SSH key:

- it's held in memory that is cleared when it goes, and logs never show it;
- it's compared in constant time;
- it's never in an address, since proxies and servers log those. A connection shows it in its
  first frame, of at most 1 KiB and within 5 seconds, or is closed with nothing else read.

A connection that speaks another protocol is refused before the WebSocket opens. A connection is
pinged every 15 seconds and dropped after 45 seconds in which nothing came from it.

## Building the page and the app

`just release-build` builds the window and edualcd with its page; it needs
[dioxus-cli](https://dioxuslabs.com) (`cargo install --locked dioxus-cli`). `cargo edualcd
--release` builds edualcd without its page.

There is no release of the Android app yet. dx builds it too. It needs the Android SDK and NDK
(`ANDROID_HOME`, `ANDROID_NDK_HOME`), a JDK 17 or later as `JAVA_HOME`, and the Rust target:

```sh
dx build --android -p edualc-server --features mobile --target aarch64-linux-android
```

Use `x86_64-linux-android` for the emulator. `dx serve` with the same arguments installs and
starts it on a running emulator or a connected phone. In the emulator, the machine it runs on is
`10.0.2.2`, so `edualcd --demo` there is `http://10.0.2.2:7878/`.
