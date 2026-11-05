// windowcast's Android host as a Shizuku user service (Droidtop/tracker#615):
// what the app that started it may ask of it, across the binder.
package dev.windowcast.host;

interface IAndroidHost {
    // Shizuku's own call to end a user service (its fixed transaction code).
    void destroy() = 16777114;

    // Starts serving on listen ("127.0.0.1:47110"), keeping its identity in a
    // folder of the shell user's; the host's windowcast id (hex), or throws.
    String start(String listen, boolean pairing) = 1;

    // Starts packageName on a display of its own; the window's id.
    long openApp(String packageName, int width, int height, int dpi) = 2;

    void closeApp(long window) = 3;
}
