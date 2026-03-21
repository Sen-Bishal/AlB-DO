pub const HYDRATION_PAYLOAD_ELEMENT_ID: &str = "__ALBEDO_HYDRATION_PAYLOAD__";
pub const HYDRATION_BOOTSTRAP_ELEMENT_ID: &str = "__ALBEDO_HYDRATION_BOOTSTRAP__";
pub const MAX_BOOTSTRAP_SCRIPT_BYTES: usize = 2048;

pub fn build_payload_script_tag(payload_json: &str, checksum: &str, version: &str) -> String {
    let escaped_payload = escape_inline_script_json(payload_json);
    let checksum_attr = escape_html_attr(checksum);
    let version_attr = escape_html_attr(version);

    format!(
        "<script id=\"{HYDRATION_PAYLOAD_ELEMENT_ID}\" type=\"application/json\" data-albedo-checksum=\"{checksum_attr}\" data-albedo-version=\"{version_attr}\">{escaped_payload}</script>"
    )
}

pub fn build_bootstrap_script_tag(expected_checksum: &str, expected_version: &str) -> String {
    let script = build_bootstrap_script(expected_checksum, expected_version);
    format!("<script id=\"{HYDRATION_BOOTSTRAP_ELEMENT_ID}\">{script}</script>")
}

pub fn build_bootstrap_script(expected_checksum: &str, expected_version: &str) -> String {
    let template = r#"(function(){var d=globalThis.document,s=d&&d.getElementById&&d.getElementById("__PAYLOAD_ID__");if(!s)return;var p;try{p=JSON.parse(s.textContent||"{}");}catch(_e){return;}if(!p||p.version!=="__VERSION__"||p.checksum!=="__CHECKSUM__")return;var h=globalThis.__ALBEDO_HYDRATE_ISLAND;var run=function(i){try{if(typeof h==="function")h(i);}catch(err){if(globalThis.console&&typeof globalThis.console.error==="function"){globalThis.console.error("ALBEDO hydrate island failed",i&&i.component_id,err);}}};var idle=function(cb){if(typeof globalThis.requestIdleCallback==="function"){return globalThis.requestIdleCallback(cb);}if(typeof globalThis.setTimeout==="function"){return globalThis.setTimeout(cb,1);}return cb();};(p.islands||[]).forEach(function(i){if(!i||!i.trigger)return;if(i.trigger==="idle"){idle(function(){run(i);});return;}if(i.trigger==="visible"){if(!d||typeof d.querySelector!=="function")return;var el=d.querySelector('[data-albedo-island="'+i.component_id+'"]');if(!el)return;if(typeof globalThis.IntersectionObserver==="function"){var obs=new globalThis.IntersectionObserver(function(es){for(var n=0;n<es.length;n++){if(es[n].isIntersecting){obs.disconnect();run(i);break;}}});obs.observe(el);}else{idle(function(){run(i);});}return;}if(i.trigger==="interaction"){if(!d||typeof d.querySelector!=="function")return;var node=d.querySelector('[data-albedo-island="'+i.component_id+'"]');if(!node||typeof node.addEventListener!=="function")return;var once=function(){run(i);};["click","keydown","pointerdown","touchstart"].forEach(function(ev){node.addEventListener(ev,once,{once:true,passive:true});});return;}run(i);});})();"#;

    let script = template
        .replace("__PAYLOAD_ID__", HYDRATION_PAYLOAD_ELEMENT_ID)
        .replace("__VERSION__", expected_version)
        .replace("__CHECKSUM__", expected_checksum);
    debug_assert!(script.len() <= MAX_BOOTSTRAP_SCRIPT_BYTES);
    script
}

fn escape_inline_script_json(payload_json: &str) -> String {
    payload_json
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
}

fn escape_html_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bootstrap_script_respects_size_budget() {
        let script = build_bootstrap_script("abc123", "1.0");
        assert!(script.len() <= MAX_BOOTSTRAP_SCRIPT_BYTES);
    }

    #[test]
    fn test_payload_script_escapes_inline_html_sensitive_chars() {
        let tag = build_payload_script_tag(r#"{"x":"</script><div>"}"#, "abc123", "1.0");
        assert!(tag.contains("\\u003c/script\\u003e"));
        assert!(!tag.contains("</script><div>"));
    }

    #[test]
    fn test_bootstrap_script_embeds_checksum_and_version() {
        let script = build_bootstrap_script("deadbeef", "1.0");
        assert!(script.contains("deadbeef"));
        assert!(script.contains("1.0"));
    }
}
