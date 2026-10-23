package dev.windowcast.viewer

/**
 * Fitting a window's picture on the screen without changing its shape.
 *
 * A stream's surface used to fill the screen whatever the window's proportions were, so a
 * 4:3 window on a 16:9 handheld came out stretched. The picture is now the largest size of the
 * window's own aspect that fits, centred, with bars around it. Plain Kotlin so it is tested
 * without a device.
 */
object Letterbox {
    /**
     * The width and height of a [pictureWidth] by [pictureHeight] picture fitted inside a
     * [boxWidth] by [boxHeight] box, keeping its shape; never larger than the box, at least one
     * pixel each way. Null when any of the four is not positive (nothing to fit yet), which
     * callers answer by filling the box.
     */
    fun fit(pictureWidth: Int, pictureHeight: Int, boxWidth: Int, boxHeight: Int): Pair<Int, Int>? {
        if (pictureWidth <= 0 || pictureHeight <= 0 || boxWidth <= 0 || boxHeight <= 0) return null
        // Compare pictureWidth/pictureHeight with boxWidth/boxHeight by cross-multiplying, in
        // Long so large sizes cannot overflow.
        val pictureWider = pictureWidth.toLong() * boxHeight >= boxWidth.toLong() * pictureHeight
        return if (pictureWider) {
            val height = (boxWidth.toLong() * pictureHeight / pictureWidth).toInt()
            boxWidth to height.coerceIn(1, boxHeight)
        } else {
            val width = (boxHeight.toLong() * pictureWidth / pictureHeight).toInt()
            width.coerceIn(1, boxWidth) to boxHeight
        }
    }
}
