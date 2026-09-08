// 固定移植来源：can1357/oh-my-pi@daf07999c2fee9b22edc7bf8fea1fb6272e0df5e。
// packages/coding-agent/src/tools/browser/launch.ts 与 tools/puppeteer/；MIT 见 stealth/LICENSE。
// 上游脚本保持逐字原样；仅把 TypeScript 模板拼接及宿主信息查询适配为 Rust。
use std::sync::{LazyLock, OnceLock};

use regex::Regex;
use serde_json::{json, Value};

const UPSTREAM_LICENSE: &str = include_str!("stealth/LICENSE");

const PATCHES: [&str; 14] = [
    include_str!("stealth/00_stealth_tampering.txt"),
    include_str!("stealth/01_stealth_activity.txt"),
    include_str!("stealth/02_stealth_hairline.txt"),
    include_str!("stealth/03_stealth_botd.txt"),
    include_str!("stealth/04_stealth_iframe.txt"),
    include_str!("stealth/05_stealth_webgl.txt"),
    include_str!("stealth/06_stealth_screen.txt"),
    include_str!("stealth/07_stealth_fonts.txt"),
    include_str!("stealth/08_stealth_audio.txt"),
    include_str!("stealth/09_stealth_locale.txt"),
    include_str!("stealth/10_stealth_plugins.txt"),
    include_str!("stealth/11_stealth_hardware.txt"),
    include_str!("stealth/12_stealth_codecs.txt"),
    include_str!("stealth/13_stealth_worker.txt"),
];

const BOOTSTRAP_PREFIX: &str = r###"(() => {
				const Page_Function_toString = Function.prototype.toString;
				const Page_FunctionToStringDescriptor = Object.getOwnPropertyDescriptor(Function.prototype, "toString");
				const Page_Proxy = Proxy;
				const Page_WeakMap = WeakMap;
				const Page_WeakMap_get = Page_WeakMap.prototype.get;
				const Page_WeakMap_set = Page_WeakMap.prototype.set;
				// Native function cache - captured before any tampering.
				// A same-origin iframe yields natives uncontaminated by page-level
				// tampering, but at document-start (when this preload runs) there is
				// no documentElement to attach it to. In that case the page itself
				// hasn't executed yet, so window's own natives are still pristine —
				// fall back to window instead of bailing, otherwise none of the
				// fingerprint patches below would ever run.
				let iframe = null;
				const container = document.head ?? document.documentElement;
				if (container) {
					iframe = document.createElement("iframe");
					iframe.style.display = "none";
					container.appendChild(iframe);
					if (!iframe.contentWindow) iframe = null;
				}
				try {
					const nativeWindow = iframe ? iframe.contentWindow : window;

					// Cache pristine native functions
					const Function_toString = nativeWindow.Function.prototype.toString;
					const Object_getOwnPropertyDescriptor = nativeWindow.Object.getOwnPropertyDescriptor;
					const Object_getOwnPropertyDescriptors = nativeWindow.Object.getOwnPropertyDescriptors;
					const Object_getPrototypeOf = nativeWindow.Object.getPrototypeOf;
					const Object_defineProperty = nativeWindow.Object.defineProperty;
					const Object_getOwnPropertyDescriptorOriginal = nativeWindow.Object.getOwnPropertyDescriptor;
					const Object_create = nativeWindow.Object.create;
					const Object_keys = nativeWindow.Object.keys;
					const Object_getOwnPropertyNames = nativeWindow.Object.getOwnPropertyNames;
					const Object_entries = nativeWindow.Object.entries;
					const Object_setPrototypeOf = nativeWindow.Object.setPrototypeOf;
					const Object_assign = nativeWindow.Object.assign;
					const Window_setTimeout = nativeWindow.setTimeout;
					const Math_random = nativeWindow.Math.random;
					const Math_floor = nativeWindow.Math.floor;
					const Math_max = nativeWindow.Math.max;
					const Math_min = nativeWindow.Math.min;
					const Window_Event = nativeWindow.Event;
					const Promise_resolve = nativeWindow.Promise.resolve.bind(nativeWindow.Promise);
					const Window_Blob = nativeWindow.Blob;
					const Window_Proxy = nativeWindow.Proxy;
					const Reflect_get = nativeWindow.Reflect.get;
					const Reflect_set = nativeWindow.Reflect.set;
					const Reflect_apply = nativeWindow.Reflect.apply;
					const Reflect_construct = nativeWindow.Reflect.construct;
					const Reflect_defineProperty = nativeWindow.Reflect.defineProperty;
					const Reflect_deleteProperty = nativeWindow.Reflect.deleteProperty;
					const Reflect_getOwnPropertyDescriptor = nativeWindow.Reflect.getOwnPropertyDescriptor;
					const Reflect_getPrototypeOf = nativeWindow.Reflect.getPrototypeOf;
					const Reflect_has = nativeWindow.Reflect.has;
					const Reflect_isExtensible = nativeWindow.Reflect.isExtensible;
					const Reflect_ownKeys = nativeWindow.Reflect.ownKeys;
					const Reflect_preventExtensions = nativeWindow.Reflect.preventExtensions;
					const Reflect_setPrototypeOf = nativeWindow.Reflect.setPrototypeOf;
					const Intl_DateTimeFormat = nativeWindow.Intl.DateTimeFormat;
					const Date_constructor = nativeWindow.Date;

					const nativeFunctionSources = new Page_WeakMap();
					const makeNativeString = (name) => "function " + (name || "") + "() { [native code] }";
					const registerNativeSource = (fn, source) => {
						if (typeof fn === "function") Reflect_apply(Page_WeakMap_set, nativeFunctionSources, [fn, source]);
						return fn;
					};
					const patchToString = (fn, name) => registerNativeSource(fn, makeNativeString(name));
					if (true) {
						const functionToStringProxy = new Page_Proxy(Page_Function_toString, {
							apply(target, thisArg, args) {
								const source = Reflect_apply(Page_WeakMap_get, nativeFunctionSources, [thisArg]);
								if (source) return source;
								return Reflect_apply(target, thisArg, args || []);
							},
							get(target, key, receiver) {
								return Reflect_get(target, key, receiver);
							},
						});
						registerNativeSource(functionToStringProxy, makeNativeString("toString"));
						Object_defineProperty(Function.prototype, "toString", {
							...(Page_FunctionToStringDescriptor || {
								writable: true,
								configurable: true,
								enumerable: false,
							}),
							value: functionToStringProxy,
						});
					}

					"###;
const BOOTSTRAP_SUFFIX: &str = r###"
				} finally {
					if (iframe && iframe.parentNode) iframe.parentNode.removeChild(iframe);
				}})();"###;

/// 返回主世界文档预加载脚本；每个上游补丁保留独立的异常边界。
pub(super) fn script() -> &'static str {
    static SCRIPT: OnceLock<String> = OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            const BEFORE: &str = "\n\t\ttry {\n\t\t\t";
            const AFTER: &str = ";\n\t\t} catch (e) {}\n\t";
            // 许可随内嵌脚本进入二进制，单独分发可执行文件时仍保留上游授权文本。
            let capacity = UPSTREAM_LICENSE.len()
                + 7
                + BOOTSTRAP_PREFIX.len()
                + BOOTSTRAP_SUFFIX.len()
                + PATCHES
                    .iter()
                    .map(|patch| patch.len() + BEFORE.len() + AFTER.len())
                    .sum::<usize>()
                + (PATCHES.len() - 1) * 2;
            let mut source = String::with_capacity(capacity);
            source.push_str("/*\n");
            source.push_str(UPSTREAM_LICENSE);
            source.push_str("\n*/\n");
            source.push_str(BOOTSTRAP_PREFIX);
            for (index, patch) in PATCHES.iter().enumerate() {
                if index != 0 {
                    source.push_str(";\n");
                }
                source.push_str(BEFORE);
                source.push_str(patch);
                source.push_str(AFTER);
            }
            source.push_str(BOOTSTRAP_SUFFIX);
            source
        })
        .as_str()
}

// 正则与上游保持一致，包括 Chrome 版本字符类中的竖线。
static UA_PATTERNS: LazyLock<[Regex; 6]> = LazyLock::new(|| {
    [
        r"\(([^)]+)\)",
        r"Chrome/([\d|.]+)",
        r"/([\d|.]+)",
        r"Android ([^;]+)",
        r"Windows NT ([\d.]+)",
        r"Android.*?;\s([^)]+)",
    ]
    .map(|pattern| Regex::new(pattern).expect("固定的上游 UA 正则必须有效"))
});

/// 从 Browser.getVersion 的 product/userAgent 生成 Network/Emulation 通用覆盖参数。
pub(super) fn user_agent_override(browser_version: &str, raw_ua: &str) -> Value {
    let mut user_agent = raw_ua.replacen("HeadlessChrome/", "Chrome/", 1);
    if user_agent.contains("Linux") && !user_agent.contains("Android") {
        user_agent = UA_PATTERNS[0]
            .replace(&user_agent, "(Windows NT 10.0; Win64; x64)")
            .into_owned();
    }
    let ua_match = UA_PATTERNS[1].captures(&user_agent);
    let browser_match = UA_PATTERNS[2].captures(browser_version);
    let ua_version = ua_match.as_ref().and_then(|m| m.get(1)).map(|m| m.as_str());
    let browser_version = browser_match
        .as_ref()
        .and_then(|m| m.get(1))
        .map(|m| m.as_str());
    let legacy_version = ua_version.or(browser_version).unwrap_or("0");
    let full_version = browser_version.unwrap_or(legacy_version);
    // JavaScript parseInt 接受数字前缀；保持上游对不规则版本字符串的处理。
    let major_digits = legacy_version
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    let major_version = legacy_version[..major_digits].parse::<u64>().unwrap_or(0);
    let is_android = user_agent.contains("Android");
    let is_mac = user_agent.contains("Mac OS X");
    let is_windows = user_agent.contains("Windows");
    let is_linux = user_agent.contains("Linux");
    let (platform, platform_full) = if is_mac {
        ("MacIntel", "macOS")
    } else if is_android {
        ("Android", "Android")
    } else if is_linux {
        ("Linux", "Linux")
    } else {
        ("Win32", "Windows")
    };
    let platform_match = if user_agent.contains("Android ") {
        UA_PATTERNS[3].captures(&user_agent)
    } else if is_windows {
        UA_PATTERNS[4].captures(&user_agent)
    } else {
        None
    };
    let platform_version = if is_mac {
        mac_os_product_version()
    } else {
        platform_match
            .as_ref()
            .and_then(|m| m.get(1))
            .map_or("", |m| m.as_str())
    };
    // Rust 的 aarch64 对应 Node 的 arm64，其余映射沿用上游宿主架构规则。
    let host_arch = std::env::consts::ARCH;
    let architecture = if is_android {
        ""
    } else if host_arch == "aarch64" {
        "arm"
    } else if host_arch.contains("64") {
        "x86"
    } else {
        ""
    };
    let bitness = if !is_android && host_arch.contains("64") {
        "64"
    } else {
        ""
    };
    let model_match = if is_android {
        UA_PATTERNS[5].captures(&user_agent)
    } else {
        None
    };
    let model = model_match
        .as_ref()
        .and_then(|m| m.get(1))
        .map_or("", |m| m.as_str());
    const ORDERS: [[usize; 3]; 6] = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];
    let order = ORDERS[(major_version % ORDERS.len() as u64) as usize];
    let escaped_chars = [" ", " ", ";"];
    let grease_brand = format!(
        "{}Not{}A{}Brand",
        escaped_chars[order[0]], escaped_chars[order[1]], escaped_chars[order[2]]
    );
    let mut brand_names = [""; 3];
    brand_names[order[0]] = &grease_brand;
    brand_names[order[1]] = "Chromium";
    brand_names[order[2]] = "Google Chrome";
    let major_version = major_version.to_string();
    let brands = brand_names.map(|brand| {
        json!({
            "brand": brand,
            "version": if brand == grease_brand { "99" } else { &major_version },
        })
    });
    let full_version_list = brand_names.map(|brand| {
        json!({
            "brand": brand,
            "version": if brand == grease_brand { "99.0.0.0" } else { full_version },
        })
    });
    json!({
        "userAgent": user_agent,
        "platform": platform,
        "acceptLanguage": "en-US,en",
        "userAgentMetadata": {
            "brands": brands,
            "fullVersion": full_version,
            "fullVersionList": full_version_list,
            "platform": platform_full,
            "platformVersion": platform_version,
            "architecture": architecture,
            "bitness": bitness,
            "model": model,
            "mobile": is_android,
        },
    })
}

fn mac_os_product_version() -> &'static str {
    // 同步接口只在首次遇到 macOS UA 时读取宿主版本；非 macOS 与读取失败均按上游返回空串。
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            if !cfg!(target_os = "macos") {
                return String::new();
            }
            let Ok(plist) =
                std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist")
            else {
                return String::new();
            };
            Regex::new(r"<key>ProductVersion</key>\s*<string>([^<]+)</string>")
                .expect("固定的上游系统版本正则必须有效")
                .captures(&plist)
                .and_then(|captures| captures.get(1))
                .map_or_else(String::new, |version| version.as_str().to_owned())
        })
        .as_str()
}
