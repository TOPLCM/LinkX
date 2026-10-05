package com.linkx.app

import android.content.Context
import android.content.SharedPreferences
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Log
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * 长期身份私钥的安全存储。
 *
 * 明文私钥落盘时，root 或 `adb backup` / 云备份都能读出并据此冒充本机身份，
 * 所以私钥用 **Android Keystore（TEE/StrongBox 优先）** 中的 AES-256-GCM 密钥加密后落盘，
 * 密文与 IV 存 SharedPreferences；密钥永不离开安全硬件。旧版明文数据在首次读取时
 * **原地迁移**（私钥字节不变 → 指纹不漂移，已配对设备不受影响），迁移后删除明文。
 */
internal object IdentityStore {
    private const val TAG = "LinkX.Identity"
    private const val KEYSTORE = "AndroidKeyStore"
    private const val KEY_ALIAS = "linkx.identity.wrap.v1"
    private const val TRANSFORM = "AES/GCM/NoPadding"
    private const val GCM_TAG_BITS = 128
    private const val IV_LEN = 12
    private const val SK_LEN = 32

    /** 加密后的私钥（base64(iv || ciphertext||tag)） */
    private const val KEY_SK_ENC = "identity_sk_enc"
    private const val KEY_SK_REV = "identity_sk_enc_rev"

    /** 旧版明文字段（仅用于一次性迁移；迁移后删除） */
    private const val KEY_SK_LEGACY = "identity_sk_hex"

    /** RSA-2048 设备身份（PKCS#8 DER，同为 Keystore AES-GCM 封装） */
    private const val KEY_DER_ENC = "device_identity_der_enc"

    /** 旧版单指纹字段（与 LinkxRuntime 的 `KEY_PEER_FP` 同名；迁移时清除） */
    private const val KEY_LEGACY_PEER = "peer_fingerprint"

    /**
     * 取得长期身份私钥（hex 字符串，与 Core 的 `sk_hex` 契约一致）。
     * 首次调用会生成新私钥；旧版明文数据会被自动迁移为加密存储。
     */
    @Synchronized
    fun getOrCreateSkHex(ctx: Context, prefs: SharedPreferences): String {
        // 1) 新格式：Keystore 解封
        prefs.getString(KEY_SK_ENC, null)?.let { enc ->
            decryptHex(enc)?.let { return it }
            Log.w(TAG, "密文解封失败（可能换机/清除过 Keystore），将重新生成身份")
        }

        // 2) 旧格式：明文 hex → 迁移为加密存储（私钥字节保持不变）
        prefs.getString(KEY_SK_LEGACY, null)?.let { legacy ->
            val sk = normalizeSkHex(legacy)
            if (sk != null) {
                if (store(prefs, sk)) {
                    prefs.edit().remove(KEY_SK_LEGACY).apply()
                    Log.i(TAG, "已把明文身份私钥迁移为 Keystore 加密存储")
                }
                return sk
            }
        }

        // 3) 首次运行：生成新私钥
        val rnd = ByteArray(SK_LEN)
        SecureRandom().nextBytes(rnd)
        val hex = toHex(rnd)
        if (store(prefs, hex)) {
            Log.i(TAG, "已生成并加密持久化长期身份私钥（32B）")
        } else {
            // 落盘失败必须大声记：未持久化 = 下次启动另生成一份 → 指纹漂移（幽灵设备成因）。
            Log.e(TAG, "身份私钥加密落盘失败（Keystore 不可用）：本次身份未持久化，重启后将更换指纹")
        }
        return hex
    }

    /** 加密并写入（成功返回 true） */
    private fun store(prefs: SharedPreferences, skHex: String): Boolean {
        val enc = encryptHex(skHex) ?: return false
        val rev = prefs.getInt(KEY_SK_REV, 0) + 1
        return prefs
            .edit()
            .putString(KEY_SK_ENC, enc)
            .putInt(KEY_SK_REV, rev)
            .commit() // 同步落盘：私钥必须先于后续握手稳定存在
    }

    /**
     * 取得 **RSA-2048 设备身份**（PKCS#8 DER 字节）。
     *
     * - 已存在 → Keystore 解封返回；
     * - 首次运行 / 旧版（只有 X25519）迁移 → 调 Core 生成新 RSA 身份并加密落盘。
     *   迁移时**清空旧的单指纹信任**（[KEY_LEGACY_PEER]）：旧指纹是 X25519 口径，
     *   对新 RSA 身份无意义；对端需重配一次（这是根治幽灵设备身份的必然代价）。
     *
     * 返回 null = Keystore 或 Core 不可用（调用方必须显式提示，**不得**用临时身份继续）。
     */
    @Synchronized
    fun getOrCreateDeviceIdentity(ctx: Context, prefs: SharedPreferences): ByteArray? {
        prefs.getString(KEY_DER_ENC, null)?.let { enc ->
            decryptBytes(enc)?.let { return it }
            Log.w(TAG, "设备身份密文解封失败（换机/清除 Keystore），将重新生成身份")
        }

        if (!NativeCore.isLoaded()) {
            Log.e(TAG, "Core 未加载，无法生成 RSA 设备身份")
            return null
        }
        val der = runCatching { NativeCore.nativeIdentityGenerate() }.getOrNull()
        if (der == null || der.isEmpty()) {
            Log.e(TAG, "RSA 设备身份生成失败")
            return null
        }
        if (!storeBytes(prefs, der)) {
            Log.e(TAG, "设备身份加密落盘失败（Keystore 不可用）")
            return null
        }
        // 迁移副作用：旧单指纹（X25519 口径）不再适用，清掉避免 UI 显示错误设备
        if (prefs.contains(KEY_LEGACY_PEER)) {
            prefs.edit().remove(KEY_LEGACY_PEER).apply()
            Log.i(TAG, "已清除 0.2.0 旧单指纹信任（新 RSA 身份须重新配对一次）")
        }
        Log.i(TAG, "已生成并加密持久化 RSA-2048 设备身份（${der.size}B PKCS#8）")
        return der
    }

    /** 本机指纹（16 位小写 hex；Core 不可用/ DER 非法返回 null） */
    fun fingerprintOf(der: ByteArray): String? =
        runCatching { NativeCore.nativeIdentityFingerprint(der) }.getOrNull()

    /** 加密任意字节并写入（成功返回 true） */
    private fun storeBytes(prefs: SharedPreferences, plain: ByteArray): Boolean {
        val enc = encryptBytes(plain) ?: return false
        return prefs.edit().putString(KEY_DER_ENC, enc).commit()
    }

    private fun encryptBytes(plain: ByteArray): String? = runCatching {
        val cipher = Cipher.getInstance(TRANSFORM)
        // AndroidKeyStore 密钥以 setRandomizedEncryptionRequired(true) 创建（见 obtainKey），
        // 加密时 IV 必须由 Keystore 自行生成；显式传 GCMParameterSpec 会抛
        // 「Caller-provided IV not permitted」，导致身份无法落盘 → 引擎拒绝启动 → 双端永远停在未配对。
        // 不得改成 setRandomizedEncryptionRequired(false)：那会放开 IV 复用，是安全倒退。
        // 解密侧仍须自带 IV，故只改加密路径。
        cipher.init(Cipher.ENCRYPT_MODE, obtainKey())
        val iv = cipher.iv
        require(iv.size == IV_LEN) { "Keystore 生成的 IV 长度异常: ${iv.size}" }
        toBase64(iv + cipher.doFinal(plain))
    }.getOrElse {
        Log.e(TAG, "加密失败: ${it.message}")
        null
    }

    private fun decryptBytes(encB64: String): ByteArray? = runCatching {
        val blob = fromBase64(encB64) ?: return@runCatching null
        if (blob.size <= IV_LEN) return@runCatching null
        val iv = blob.copyOfRange(0, IV_LEN)
        val ct = blob.copyOfRange(IV_LEN, blob.size)
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.DECRYPT_MODE, obtainKey(), GCMParameterSpec(GCM_TAG_BITS, iv))
        cipher.doFinal(ct)
    }.getOrNull()

    private fun encryptHex(skHex: String): String? = runCatching {
        val cipher = Cipher.getInstance(TRANSFORM)
        // 同 encryptBytes：IV 由 Keystore 生成，不得自带。
        cipher.init(Cipher.ENCRYPT_MODE, obtainKey())
        val iv = cipher.iv
        require(iv.size == IV_LEN) { "Keystore 生成的 IV 长度异常: ${iv.size}" }
        val ct = cipher.doFinal(skHex.toByteArray(Charsets.US_ASCII))
        toBase64(iv + ct)
    }.getOrElse {
        Log.e(TAG, "加密失败: ${it.message}")
        null
    }

    private fun decryptHex(encB64: String): String? = runCatching {
        val blob = fromBase64(encB64) ?: return@runCatching null
        if (blob.size <= IV_LEN) return@runCatching null
        val iv = blob.copyOfRange(0, IV_LEN)
        val ct = blob.copyOfRange(IV_LEN, blob.size)
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.DECRYPT_MODE, obtainKey(), GCMParameterSpec(GCM_TAG_BITS, iv))
        val plain = cipher.doFinal(ct)
        normalizeSkHex(String(plain, Charsets.US_ASCII))
    }.getOrNull()

    /** Keystore 中取（或创建）封装密钥；禁用备份以配合关闭 allowBackup */
    private fun obtainKey(): SecretKey {
        val ks = KeyStore.getInstance(KEYSTORE).apply { load(null) }
        (ks.getEntry(KEY_ALIAS, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }

        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, KEYSTORE)
        gen.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .setRandomizedEncryptionRequired(true)
                // 身份封装密钥不得进入备份：换机应重新配对，而非复制身份。
                .setUserAuthenticationRequired(false)
                .build(),
        )
        return gen.generateKey()
    }

    // ---------- 小工具（避免引入额外依赖） ----------

    /** 校验并规范化为 64 位小写 hex；非法返回 null */
    private fun normalizeSkHex(raw: String): String? {
        val s = raw.trim().lowercase()
        if (s.length != SK_LEN * 2) return null
        if (!s.all { it in "0123456789abcdef" }) return null
        return s
    }

    private fun toHex(bytes: ByteArray): String = buildString(bytes.size * 2) {
        for (b in bytes) {
            val v = b.toInt() and 0xFF
            append(HEX[v ushr 4])
            append(HEX[v and 0x0F])
        }
    }

    private const val HEX = "0123456789abcdef"

    private fun toBase64(bytes: ByteArray): String =
        android.util.Base64.encodeToString(bytes, android.util.Base64.NO_WRAP)

    private fun fromBase64(s: String): ByteArray? = runCatching {
        android.util.Base64.decode(s, android.util.Base64.NO_WRAP)
    }.getOrNull()
}