cmake_minimum_required(VERSION 3.22.1)
project(windowcast_jni C)

# client-core, cross-compiled by `cargo ndk` into src/main/jniLibs before
# Gradle runs; Gradle packages it from there, this links against it.
add_library(windowcast_client SHARED IMPORTED)
set_target_properties(windowcast_client PROPERTIES
    IMPORTED_LOCATION ${CMAKE_SOURCE_DIR}/../jniLibs/${ANDROID_ABI}/libwindowcast_client.so)

add_library(windowcast_jni SHARED windowcast_jni.c)
target_include_directories(windowcast_jni PRIVATE ${WINDOWCAST_INCLUDE})
target_link_libraries(windowcast_jni windowcast_client log)
