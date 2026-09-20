// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.DataInputStream
import java.io.IOException

/**
 * Cross-language protocol conformance test.
 *
 * Reads `testdata/protocol/corpus.tsv` from the test classpath and
 * validates the Kotlin implementation against it. The Rust implementation
 * is validated against the same file by `crates/protocol/tests/conformance.rs`.
 *
 * The corpus is regenerated with `cargo run -p protocol --bin gen_vectors`.
 * A version bump that misses one side is exactly what this test exists to
 * catch: the two implementations are frozen to the same bytes.
 */
class ProtocolConformanceTest {

    private data class Case(
        val name: String,
        val kind: String,
        val direction: String,
        val expect: String,
        val fields: Map<String, String>,
        val errSubstr: String,
        val bytes: ByteArray,
    )

    private fun loadCorpus(): List<Case> {
        // `javaClass.getResourceAsStream` with a leading slash resolves the
        // path from the classpath root, which is where the Gradle test
        // resource source set puts `testdata/`. Using the classloader
        // directly would be equivalent, but `ClassLoader` is nullable in
        // Kotlin and the call site would need a `?.` that only obscures the
        // intent.
        val stream = javaClass.getResourceAsStream("/protocol/corpus.tsv")
            ?: error("protocol/corpus.tsv not on the test classpath; check the " +
                     "sourceSets.test.resources.srcDirs entry in app/build.gradle.kts")
        val cases = mutableListOf<Case>()
        stream.bufferedReader().useLines { lines ->
            for ((lineno, raw) in lines.withIndex()) {
                if (raw.isBlank() || raw.startsWith("#")) continue
                val cols = raw.split('\t')
                require(cols.size == 6) { "line ${lineno + 1} has ${cols.size} columns" }
                val hex = cols[5]
                require(hex.length % 2 == 0) { "odd hex on line ${lineno + 1}" }
                val bytes = ByteArray(hex.length / 2) { i ->
                    hex.substring(i * 2, i * 2 + 2).toInt(16).toByte()
                }
                val expect = cols[3]
                val fields = mutableMapOf<String, String>()
                var errSubstr = ""
                if (expect == "ok") {
                    for (pair in cols[4].split(';')) {
                        if (pair.isEmpty()) continue
                        val (k, v) = pair.split('=', limit = 2)
                        fields[k] = v
                    }
                } else {
                    errSubstr = cols[4]
                }
                cases.add(Case(cols[0], cols[1], cols[2], expect, fields, errSubstr, bytes))
            }
        }
        return cases
    }

    @Test
    fun kotlinParsesHostToDeviceCases() {
        val cases = loadCorpus()
        assertTrue("corpus is empty", cases.isNotEmpty())
        val failures = mutableListOf<String>()
        for (c in cases) {
            // Only h2d cases are parsed by Kotlin. The device-to-host cases
            // are covered by the encoder test below and by the Rust side.
            if (c.kind !in setOf("stream_header", "frame_header", "control")) continue
            val outcome = try {
                parse(c.kind, c.bytes)
            } catch (e: IOException) {
                if (c.expect != "err") {
                    failures.add("${c.name}: expected ok, got ${e.message}")
                } else if (c.errSubstr.isNotEmpty()
                        && !e.message.orEmpty().contains(c.errSubstr)) {
                    failures.add("${c.name}: error ${e.message} missing ${c.errSubstr}")
                }
                continue
            }
            if (c.expect != "ok") {
                failures.add("${c.name}: expected err, got ok")
                continue
            }
            for ((k, want) in c.fields) {
                val got = outcome[k]
                if (got != want) failures.add("${c.name}: field $k want=$want got=$got")
            }
        }
        if (failures.isNotEmpty()) {
            failures.forEach { System.err.println("conformance: $it") }
            fail("${failures.size} conformance failures:\n" + failures.joinToString("\n"))
        }
    }

    @Test
    fun kotlinEncodesDeviceToHostCases() {
        val cases = loadCorpus()
        val failures = mutableListOf<String>()
        for (c in cases) {
            if (c.direction != "d2h" || c.expect != "ok") continue
            val encoded = when (c.kind) {
                "ack" -> Protocol.encodeAckMessage(c.fields["pts_ns"]!!.toULong().toLong())
                "touch" -> {
                    val action = when (c.fields["action"]) {
                        "Down" -> Protocol.TouchAction.DOWN
                        "Move" -> Protocol.TouchAction.MOVE
                        "Up" -> Protocol.TouchAction.UP
                        "Cancel" -> Protocol.TouchAction.CANCEL
                        else -> { failures.add("${c.name}: bad action"); continue }
                    }
                    Protocol.encodeTouchMessage(
                        Protocol.TouchMessage(
                            action,
                            c.fields["x"]!!.toInt(),
                            c.fields["y"]!!.toInt(),
                        )
                    )
                }
                else -> continue
            }
            if (!encoded.contentEquals(c.bytes)) {
                failures.add("${c.name}: encoded ${encoded.toHex()} vs corpus ${c.bytes.toHex()}")
            }
        }
        if (failures.isNotEmpty()) {
            failures.forEach { System.err.println("conformance-encode: $it") }
            fail("${failures.size} encode failures:\n" + failures.joinToString("\n"))
        }
    }

    private fun parse(kind: String, bytes: ByteArray): Map<String, String> {
        val out = mutableMapOf<String, String>()
        when (kind) {
            "stream_header" -> {
                val h = Protocol.readStreamHeader(DataInputStream(ByteArrayInputStream(bytes)))
                out["width"] = h.width.toString()
                out["height"] = h.height.toString()
                out["framerate"] = h.framerate.toString()
                out["codec"] = if (h.mime == "video/avc") "H264" else "H265"
            }
            "frame_header" -> {
                val h = Protocol.readFrameHeader(DataInputStream(ByteArrayInputStream(bytes)))
                out["length"] = h.length.toString()
                out["pts_ns"] = h.ptsNs.toString()
                out["keyframe"] = h.keyframe.toString()
                out["control"] = h.control.toString()
            }
            "control" -> {
                val c = Protocol.decodeControl(bytes, bytes.size)
                out["kind"] = when (c.kind) {
                    Protocol.ControlKind.BRIGHTNESS -> "Brightness"
                    Protocol.ControlKind.ROTATION -> "Rotation"
                }
                out["value"] = c.value.toString()
            }
            else -> throw IOException("unknown kind $kind")
        }
        return out
    }

    private fun ByteArray.toHex() = joinToString("") { "%02x".format(it) }
}
