# Newfoundsync for Android TV / Google TV

A small app for the TV: open it with the remote and it finds the Newfoundsync server on your network
and plays in sync, full screen, with the logo and visualizer. The TV then shows up in the server
window's Devices list like any other device (volume, mute, sync, remembered by name).

It is a thin shell: a full-screen WebView on the server's own page, so all the sync logic is the code
every other device runs.

- **Sound and video:** it opens the HTTPS page (`https://<server>:47000/?app=1…`), where the WebView
  has WebCodecs. Video, when the server shares a screen or a phone casts a tab, fills the screen;
  otherwise the logo and visualizer show. The server's certificate is self-signed, and the app
  accepts it only when its SHA-256 matches the `certSha256` the server reports in `/hello`.
- **Fallback, sound only:** if that page cannot load, the plain-HTTP TV page
  (`http://<server>:47001/tv?app=1…`), which the server feeds
  PCM. The HTTPS page also sends itself there if the WebView turns out to have no WebCodecs.

## Finding the server

1. The server it played from last time, if it still answers.
2. Otherwise every address in the TV's /24 on port 47001, asking `GET /hello` for
   `{"app":"newfoundsync","name":…,"httpsPort":47000,"certSha256":…}`. One found: it connects. Several: a list to choose from. None: a box
   for the PC's address.

It does not use mDNS, because many home networks drop multicast between wired and wireless devices.

## Build

```bash
android/build.sh
```

Needs a JDK 17 and an Android SDK with `build-tools;35.0.0` and `platforms;android-35`
(`ANDROID_SDK_ROOT`, default `~/Android/Sdk`). No Gradle. The APK lands at
`android/build/newfoundsync-tv.apk`, signed with a key kept outside the repo
(`~/.android/newfoundsync-tv.keystore`, created on the first build). Keep that key: an update signed
with a different one will not install over the app.

On the dev box the JDK is the Flathub `org.freedesktop.Sdk.Extension.openjdk17` extension:

```bash
flatpak run --user --filesystem=$PWD --filesystem=$HOME/Android --filesystem=$HOME/.android \
  --command=sh org.freedesktop.Sdk//25.08 \
  -c 'export JAVA_HOME=$(dirname $(dirname $(readlink -f /usr/lib/sdk/openjdk17/bin/javac))); ./android/build.sh'
```

## Install on the TV (sideload, free)

On a Google TV: Settings → System → About, select **Android TV OS build** seven times to unlock
Developer options; then in Developer options turn on **USB debugging** and **Wireless/Network
debugging**. From the PC:

```bash
adb connect <tv-ip>:5555
adb install -r android/build/newfoundsync-tv.apk
```

Allow the connection on the TV the first time. The app appears under Apps (Your apps); it can be
pinned to the home screen. Without developer mode, copy the APK over with a USB stick or a "send
files to TV" app and open it with a file manager (allow installing unknown apps when asked).

## Limits

- It plays while the app is open. Home pauses it (Android pauses the WebView).
- The TV's own picture and sound processing adds delay the app cannot see: set its sync by ear
  once in the server window; it is remembered.
