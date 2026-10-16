//! Custom elements in a parsed page: the registry, upgrades, and the
//! reactions to what script does to a defined element.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use otlyra_html::HtmlParser;
use otlyra_script::PageScripts;
use otter_runtime::{ConsoleLevel, ConsoleSink, ConsoleSinkHandle};

#[derive(Debug, Default)]
struct Captured {
    lines: Mutex<Vec<String>>,
}

impl ConsoleSink for Captured {
    fn write(&self, _level: ConsoleLevel, fields: &[String]) {
        self.lines
            .lock()
            .expect("no test panics while holding this")
            .push(fields.join(" "));
    }
}

/// Parse `html` to the end, let what it deferred run, and return the console.
fn run(html: &str) -> Vec<String> {
    let sink = Arc::new(Captured::default());
    let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
    let mut parser = HtmlParser::new().with_script_runner(Box::new(PageScripts::with_console(
        "https://example.com/",
        handle,
    )));
    let mut document = otlyra_dom::Document::new();
    parser.feed(&mut document, html.into());
    while parser.blocked_on().is_some() {
        parser.skip_script(&mut document);
    }
    if let Some(mut runner) = parser.finish(&mut document) {
        runner.settle(&mut document, Duration::from_millis(100));
    }
    sink.lines
        .lock()
        .expect("no test panics while holding this")
        .clone()
}

const LOGGING: &str = "<script>\
    class Logged extends HTMLElement {\
      static observedAttributes = ['state', 'class', 'id'];\
      constructor() { super(); console.log('constructed', this.localName, this.isConnected); }\
      connectedCallback() { console.log('connected', this.id || this.localName); }\
      disconnectedCallback() { console.log('disconnected', this.id || this.localName); }\
      attributeChangedCallback(name, old, value) { console.log('changed', name, old, value); }\
    }\
    </script>";

#[test]
fn elements_parsed_before_the_definition_are_upgraded_by_it() {
    let log = run(&format!(
        "{LOGGING}<x-logged id=early state=on>text</x-logged>\
         <script>\
           const early = document.getElementById('early');\
           console.log('before', early instanceof Logged);\
           customElements.define('x-logged', Logged);\
           console.log('after', early instanceof Logged, early.constructor === Logged);\
         </script>"
    ));
    assert_eq!(
        log,
        [
            "before false",
            "constructed x-logged true",
            "changed state null on",
            "changed id null early",
            "connected early",
            "after true true",
        ]
    );
}

#[test]
fn elements_parsed_after_the_definition_are_upgraded_before_script_meets_them() {
    let log = run(&format!(
        "{LOGGING}<script>customElements.define('x-logged', Logged)</script>\
         <x-logged id=late></x-logged>\
         <script>console.log('script', document.getElementById('late') instanceof Logged)</script>"
    ));
    assert_eq!(
        log,
        [
            "constructed x-logged true",
            "changed id null late",
            "connected late",
            "script true"
        ]
    );
}

#[test]
fn script_made_elements_are_constructed_connected_moved_and_disconnected() {
    let log = run(&format!(
        "{LOGGING}<div id=one></div><div id=two></div>\
         <script>\
           customElements.define('x-logged', Logged);\
           const made = document.createElement('x-logged');\
           console.log('made', made instanceof Logged);\
           made.id = 'made';\
           document.getElementById('one').appendChild(made);\
           document.getElementById('two').append(made);\
           made.remove();\
           const direct = new Logged();\
           console.log('direct', direct.localName, direct instanceof HTMLElement);\
         </script>"
    ));
    assert_eq!(
        log,
        [
            "constructed x-logged false",
            "made true",
            "changed id null made",
            "connected made",
            "disconnected made",
            "connected made",
            "disconnected made",
            "constructed x-logged false",
            "direct x-logged true",
        ]
    );
}

#[test]
fn observed_attributes_report_every_way_they_change() {
    let log = run(&format!(
        "{LOGGING}<x-logged id=watched></x-logged>\
         <script>\
           customElements.define('x-logged', Logged);\
           const watched = document.getElementById('watched');\
           watched.setAttribute('STATE', 'a');\
           watched.setAttribute('ignored', 'x');\
           watched.className = 'big';\
           watched.classList.add('red');\
           watched.toggleAttribute('state');\
           watched.removeAttribute('state');\
         </script>"
    ));
    assert_eq!(
        &log[3..],
        [
            "changed state null a",
            "changed class null big",
            "changed class big big red",
            "changed state a null",
        ]
    );
}

#[test]
fn markup_creates_upgraded_elements_and_cloning_upgrades_the_copy() {
    let log = run(&format!(
        "{LOGGING}<div id=host></div>\
         <script>\
           customElements.define('x-logged', Logged);\
           const host = document.getElementById('host');\
           host.innerHTML = '<x-logged id=inner></x-logged>';\
           console.log('inner', host.firstChild instanceof Logged);\
           const copy = host.firstChild.cloneNode();\
           console.log('copy', copy instanceof Logged, copy.isConnected);\
           host.innerHTML = '';\
         </script>"
    ));
    assert_eq!(
        log,
        [
            "constructed x-logged true",
            "changed id null inner",
            "connected inner",
            "inner true",
            "constructed x-logged false",
            "changed id null inner",
            "copy true false",
            "disconnected inner",
        ]
    );
}

#[test]
fn the_registry_answers_and_refuses_as_the_platform_does() {
    let log = run("<script>\
           class A extends HTMLElement {}\
           customElements.whenDefined('x-a').then((c) => console.log('whenDefined', c === A));\
           customElements.define('x-a', A);\
           console.log(customElements.get('x-a') === A, customElements.getName(A), customElements.get('x-b'));\
           const name = (f) => { try { f(); return 'none'; } catch (e) { return e.name; } };\
           console.log(name(() => customElements.define('x-a', class extends HTMLElement {})));\
           console.log(name(() => customElements.define('nodash', class extends HTMLElement {})));\
           console.log(name(() => customElements.define('x-c', A)));\
           console.log(name(() => new HTMLElement()));\
           console.log(customElements instanceof CustomElementRegistry);\
         </script>");
    assert_eq!(
        log,
        [
            "true x-a undefined",
            "NotSupportedError",
            "SyntaxError",
            "NotSupportedError",
            "TypeError",
            "true",
            "whenDefined true",
        ]
    );
}

#[test]
fn a_constructor_that_throws_is_reported_and_the_page_goes_on() {
    let log = run(
        "<script>window.onerror = (message) => { console.log('onerror:', message); return true; }</script>\
         <x-broken></x-broken>\
         <script>\
           customElements.define('x-broken', class extends HTMLElement {\
             constructor() { super(); throw new Error('nope'); }\
             connectedCallback() { console.log('never'); }\
           });\
           console.log('still here');\
         </script>",
    );
    assert_eq!(log, ["onerror: Uncaught Error: nope", "still here"]);
}

#[test]
fn nodes_keep_their_interface_after_a_page_replaces_the_global() {
    let log = run("<script>\
           const original = HTMLElement;\
           window.HTMLElement = function HTMLElement() { return Reflect.construct(original, [], this.constructor); };\
           HTMLElement.prototype = original.prototype;\
           Object.setPrototypeOf(HTMLElement, original);\
         </script>\
         <header id=h data-x=1></header>\
         <script>\
           const header = document.getElementById('h');\
           console.log(typeof header.getAttribute, header.getAttribute('data-x'), header instanceof original);\
           customElements.define('x-late', class extends HTMLElement { connectedCallback() { console.log('late', this.localName); } });\
           document.body.appendChild(document.createElement('x-late'));\
         </script>");
    assert_eq!(log, ["function 1 true", "late x-late"]);
}

#[test]
fn shadow_roots_are_attached_filled_slotted_and_reached_through() {
    let log = run(
        "<div id=host><span id=light>light</span><b slot=x>named</b></div>\
         <section id=declared><template shadowrootmode=open><p id=inside>declared</p></template></section>\
         <script>\
           const host = document.getElementById('host');\
           const root = host.attachShadow({ mode: 'open' });\
           root.innerHTML = '<slot></slot><slot name=x></slot><button id=inner>go</button>';\
           const inner = root.getElementById('inner');\
           const [main, named] = root.querySelectorAll('slot');\
           console.log(root instanceof ShadowRoot, root instanceof DocumentFragment, host.shadowRoot === root, root.host === host, root.mode);\
           console.log(inner.isConnected, inner.getRootNode() === root, inner.getRootNode({ composed: true }) === document);\
           console.log(main.assignedNodes().map((n) => n.id).join(), named.assignedElements().map((n) => n.textContent).join(),\
             document.getElementById('light').assignedSlot === main, document.querySelector('#inner'));\
           document.addEventListener('click', (e) => console.log('document heard', e.composedPath().includes(host)));\
           inner.click();\
           let refused;\
           try { host.attachShadow({ mode: 'open' }); } catch (e) { refused = e.name; }\
           try { document.createElement('img').attachShadow({ mode: 'open' }); } catch (e) { refused += ' ' + e.name; }\
           console.log(refused, document.getElementById('declared').shadowRoot.querySelector('#inside').textContent);\
           customElements.define('x-card', class extends HTMLElement {\
             constructor() { super(); this.attachShadow({ mode: 'closed' }).innerHTML = '<x-part></x-part>'; }\
             connectedCallback() { console.log('card connected', this.shadowRoot); }\
           });\
           customElements.define('x-part', class extends HTMLElement { connectedCallback() { console.log('part connected', this.isConnected); } });\
           document.body.appendChild(document.createElement('x-card'));\
         </script>",
    );
    assert_eq!(
        log,
        [
            "true true true true open",
            "true true true",
            "light named true null",
            "document heard true",
            "NotSupportedError NotSupportedError declared",
            "card connected null",
            "part connected true",
        ]
    );
}

#[test]
fn constructed_sheets_and_parsed_markup() {
    let log = run("<div id=host></div>\
         <script>\
           const sheet = new CSSStyleSheet();\
           sheet.replaceSync(':host { color: red } p { margin: 0 }');\
           const root = document.getElementById('host').attachShadow({ mode: 'open' });\
           root.adoptedStyleSheets = [sheet];\
           sheet.insertRule('b { font-weight: 700 }', 2);\
           console.log(root.adoptedStyleSheets.length, sheet.cssRules.length, sheet.cssRules[2].cssText);\
           const parsed = new DOMParser().parseFromString('<html><head><title> T </title></head><body><p id=a>one</p><script>console.log(\"ran\")<\\/script></body></html>', 'text/html');\
           console.log(parsed.title, parsed.body.querySelector('#a').textContent, parsed.getElementById('a').id, parsed.documentElement.localName);\
           const svg = new DOMParser().parseFromString('<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"2\"/></svg>', 'image/svg+xml');\
           console.log(svg.documentElement.localName, svg.querySelector('circle').getAttribute('r'));\
           const inert = document.implementation.createHTMLDocument('inert');\
           inert.body.innerHTML = '<i>x</i>';\
           console.log(inert.title, inert.body.firstChild.localName, inert.createElement('b').isConnected, document.implementation instanceof DOMImplementation);\
         </script>");
    assert_eq!(
        log,
        [
            "1 3 b { font-weight: 700 }",
            "T one a html",
            "svg 2",
            "inert i false true"
        ]
    );
}

#[test]
fn only_upgraded_custom_elements_are_defined() {
    let log = run(
        "<x-later id=later></x-later><x-broken id=broken></x-broken>\
         <script>\
           window.onerror = () => true;\
           const defined = (id) => document.getElementById(id).matches(':defined');\
           console.log(document.body.matches(':defined'), defined('later'), document.querySelectorAll(':not(:defined)').length);\
           customElements.define('x-later', class extends HTMLElement {});\
           customElements.define('x-broken', class extends HTMLElement { constructor() { super(); throw 1; } });\
           const made = document.createElement('x-later');\
           console.log(defined('later'), defined('broken'), made.matches(':defined'), document.createElement('x-none').matches(':defined'));\
         </script>",
    );
    assert_eq!(log, ["true false 2", "true false true false"]);
}
