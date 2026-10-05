# LinkX Android 混淆规则：JNI 类与方法名必须保留，否则 release 包调用核心会静默失败
-keep class com.linkx.app.NativeCore { *; }
-keepclasseswithmembernames class * {
    native <methods>;
}