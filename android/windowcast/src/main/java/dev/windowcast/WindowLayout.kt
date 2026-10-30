package dev.windowcast

/**
 * Where and how big a client draws a window that floats over its screen: an extra window watched
 * beside the primary, or a popup, dialog or menu. The viewer and droidtop place them the same way.
 * Plain Kotlin on purpose, so the rules are tested without a device.
 */
object WindowLayout {
    /** A rectangle in screen pixels. */
    data class Box(val x: Int, val y: Int, val width: Int, val height: Int)

    /** The smallest a floating window is drawn, so a tiny or unsized window can still be seen. */
    const val MIN_FLOATING = 64

    /** The smallest a popup placed against its owner is drawn. */
    const val MIN_PLACED = 32

    /**
     * The size to draw a [width] by [height] window at on a [screenWidth] by [screenHeight] screen:
     * its own size, shrunk (never enlarged) to fit within 90% of the width and 80% of the height,
     * keeping its shape, and at least [MIN_FLOATING] each way.
     */
    fun floatingSize(width: Int, height: Int, screenWidth: Int, screenHeight: Int): Pair<Int, Int> {
        val scale = minOf(
            1f,
            screenWidth * 0.9f / width.coerceAtLeast(1),
            screenHeight * 0.8f / height.coerceAtLeast(1),
        )
        return (width * scale).toInt().coerceAtLeast(MIN_FLOATING) to
            (height * scale).toInt().coerceAtLeast(MIN_FLOATING)
    }

    /**
     * Where a [popup] goes so it sits against its [owner] as the owner is drawn on screen now, from
     * the two windows' positions on the host's desktop: the owner is shown in a box whose top-left is
     * ([originX], [originY]) and which is [shownWidth] by [shownHeight]. Null when the host gave the
     * position of either window, so the caller sizes the popup alone with [floatingSize].
     */
    fun againstOwner(
        popup: WindowInfo,
        owner: WindowInfo,
        originX: Int,
        originY: Int,
        shownWidth: Int,
        shownHeight: Int,
    ): Box? {
        val (px, py) = popup.position ?: return null
        val (ox, oy) = owner.position ?: return null
        val sx = shownWidth.toFloat() / owner.width.coerceAtLeast(1)
        val sy = shownHeight.toFloat() / owner.height.coerceAtLeast(1)
        return Box(
            x = originX + ((px - ox) * sx).toInt(),
            y = originY + ((py - oy) * sy).toInt(),
            width = (popup.width * sx).toInt().coerceAtLeast(MIN_PLACED),
            height = (popup.height * sy).toInt().coerceAtLeast(MIN_PLACED),
        )
    }
}
