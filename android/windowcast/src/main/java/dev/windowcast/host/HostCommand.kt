package dev.windowcast.host

/**
 * What windowcast's host asks of the Android side (agent-android `Command`): start or stop
 * encoding a window, a keyframe, a bitrate. Parsed from the host's flat JSON by hand, so it is
 * plain Kotlin that unit tests run.
 */
sealed class HostCommand {
    abstract val window: Long

    data class Open(override val window: Long, val codec: String) : HostCommand()
    data class Close(override val window: Long) : HostCommand()
    data class Keyframe(override val window: Long) : HostCommand()
    data class Bitrate(override val window: Long, val bitrate: Int) : HostCommand()

    /** An input event for [window]'s display, as the host's JSON (`{"PointerMove":{...}}`). */
    data class Input(override val window: Long, val event: String) : HostCommand()

    companion object {
        private fun string(json: String, key: String): String? =
            Regex("\"$key\"\\s*:\\s*\"([^\"]*)\"").find(json)?.groupValues?.get(1)

        private fun number(json: String, key: String): Long? =
            Regex("\"$key\"\\s*:\\s*(\\d+)").find(json)?.groupValues?.get(1)?.toLongOrNull()

        /** The object after `"event":`, to the command's closing brace. */
        private fun event(json: String): String? {
            val at = Regex("\"event\"\\s*:\\s*").find(json) ?: return null
            val end = json.lastIndexOf('}')
            return json.substring(at.range.last + 1, end).trim().takeIf { it.startsWith("{") }
        }

        /** The command in [json], or null for one this side does not know. */
        fun parse(json: String): HostCommand? {
            val window = number(json, "window") ?: return null
            return when (string(json, "command")) {
                "open" -> Open(window, string(json, "codec") ?: return null)
                "close" -> Close(window)
                "keyframe" -> Keyframe(window)
                "bitrate" -> Bitrate(window, number(json, "bitrate")?.toInt() ?: return null)
                "input" -> Input(window, event(json) ?: return null)
                else -> null
            }
        }

        /** The MediaCodec MIME type of a host codec name. */
        fun mime(codec: String): String? = when (codec) {
            "h264" -> "video/avc"
            "h265" -> "video/hevc"
            "av1" -> "video/av01"
            else -> null
        }
    }
}

/** The window list the host takes, as JSON: one app per virtual display. */
internal fun windowsJson(apps: Collection<HostApp>): String =
    apps.joinToString(",", "[", "]") { app ->
        val title = app.title.replace("\\", "\\\\").replace("\"", "\\\"")
        "{\"id\":${app.window},\"title\":\"$title\",\"package\":\"${app.packageName}\",\"width\":${app.width},\"height\":${app.height}}"
    }

/** One app on a display of its own, as the window list names it. */
data class HostApp(val window: Long, val packageName: String, val title: String, val width: Int, val height: Int)
