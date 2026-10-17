//! The DOM a page's script sees, checked from inside a page.
//!
//! Each test parses a document whose script runs a list of checks and prints
//! one line per check — `ok name` or `FAIL name: got … want …` — and the test
//! asserts that every line is an `ok` and that every check ran. A check is a
//! line of JavaScript beside the behaviour it pins, which is how the platform's
//! own tests read; a failure names the check and shows both values.

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
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        self.lines
            .lock()
            .expect("no test panics while holding this")
            .push(format!("{level:?}: {}", fields.join(" ")));
    }
}

/// The harness every page below starts with.
const HARNESS: &str = r"<script>
  const same = (a, b) => Object.is(a, b) || (typeof a === 'object' && a !== null && JSON.stringify(a) === JSON.stringify(b));
  const check = (name, actual, expected) => {
    console.log(same(actual, expected) ? 'ok ' + name : 'FAIL ' + name + ': got ' + JSON.stringify(actual) + ' want ' + JSON.stringify(expected));
  };
  const throwsDom = (name, run, errorName) => {
    try { run(); console.log('FAIL ' + name + ': did not throw'); }
    catch (error) {
      check(name, [error instanceof DOMException, error.name], [true, errorName]);
    }
  };
</script>";

/// Parse `body` after the harness, run its scripts to a standstill, and return
/// what they printed.
fn run(body: &str) -> Vec<String> {
    let sink = Arc::new(Captured::default());
    let handle: ConsoleSinkHandle = Arc::clone(&sink) as ConsoleSinkHandle;
    let mut parser = HtmlParser::new().with_script_runner(Box::new(PageScripts::with_console(
        "https://example.com/dir/page.html",
        handle,
    )));
    let mut document = otlyra_dom::Document::new();
    let html = format!("<!doctype html><html><head>{HARNESS}</head><body>{body}</body></html>");
    parser.feed(&mut document, html.as_str().into());
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

/// Every line an `ok`, and `expected` of them.
fn assert_all_ok(lines: &[String], expected: usize) {
    let failures: Vec<&String> = lines
        .iter()
        .filter(|line| !line.starts_with("Log: ok "))
        .collect();
    assert!(
        failures.is_empty(),
        "failed checks:\n{failures:#?}\nall:\n{lines:#?}"
    );
    assert_eq!(lines.len(), expected, "not every check ran: {lines:#?}");
}

#[test]
fn inner_html_replaces_nodes_and_serializes_markup_without_losing_old_wrappers() {
    let lines = run(r#"<div id=mount><i id=old>old</i></div><script>
  const mount = document.getElementById('mount');
  const old = document.getElementById('old');
  old.saved = 42;
  mount.innerHTML = '<p title="&quot;&amp;&lt;&gt;">A &amp; B &lt; C&nbsp;</p><!--note--><br>';
  check('innerHTML is a DOM setter', mount.firstChild instanceof HTMLParagraphElement, true);
  check('new nodes belong to the page', mount.firstChild.ownerDocument, document);
  check('old nodes are detached', old.parentNode, null);
  check('old wrappers remain usable', [old.textContent, old.saved], ['old', 42]);
  check('serialized attributes and text', mount.innerHTML, '<p title="&quot;&amp;&lt;&gt;">A &amp; B &lt; C&nbsp;</p><!--note--><br>');
  check('outerHTML includes the element', mount.firstChild.outerHTML, '<p title="&quot;&amp;&lt;&gt;">A &amp; B &lt; C&nbsp;</p>');
  const script = document.createElement('script');
  script.innerHTML = 'if (a < b && c > d) {}';
  check('script text is serialized literally', script.innerHTML, 'if (a < b && c > d) {}');
  const textarea = document.createElement('textarea');
  textarea.innerHTML = '<b>&amp;';
  check('RCDATA uses the context element', [textarea.childElementCount, textarea.textContent], [0, '<b>&']);
  check('RCDATA serialization escapes markup', textarea.innerHTML, '&lt;b&gt;&amp;');
  mount.innerHTML = null;
  check('null is the empty string', mount.childNodes.length, 0);
  mount.innerHTML = undefined;
  check('undefined is a DOMString', mount.textContent, 'undefined');
  const image = document.createElement('img');
  image.append('not serialized');
  check('void elements have no serialized contents', image.outerHTML, '<img>');
</script>"#);
    assert_all_ok(&lines, 12);
}

#[test]
fn markup_parsing_keeps_table_svg_mathml_and_template_contexts() {
    let lines = run(r#"<div id=mount></div><script>
  const table = document.createElement('table');
  table.innerHTML = '<tr><td>cell</td></tr>';
  check('table parsing inserts tbody', table.firstChild.tagName, 'TBODY');
  check('table cells are retained', table.querySelector('td').textContent, 'cell');
  const row = document.createElement('tr');
  row.innerHTML = '<td>one</td><td>two</td>';
  check('row parsing keeps cells', [row.children.length, row.firstChild.tagName], [2, 'TD']);
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.innerHTML = '<circle viewBox="0 0 10 10"></circle><foreignObject><div>HTML</div></foreignObject>';
  check('SVG children keep their interface', svg.firstChild instanceof SVGCircleElement, true);
  check('SVG attribute case survives', svg.firstChild.getAttribute('viewBox'), '0 0 10 10');
  check('foreignObject is an HTML integration point', svg.lastChild.firstChild instanceof HTMLDivElement, true);
  const annotation = document.createElementNS('http://www.w3.org/1998/Math/MathML', 'annotation-xml');
  annotation.setAttribute('encoding', 'text/html');
  annotation.innerHTML = '<span>HTML</span>';
  check('context attributes select the MathML integration point', annotation.firstChild instanceof HTMLSpanElement, true);
  const template = document.createElement('template');
  template.innerHTML = '<p>template</p><template><b>nested</b></template>';
  check('template insertion targets content', template.childNodes.length, 0);
  check('template content is populated', template.content.firstChild.textContent, 'template');
  check('nested templates are imported', template.content.lastChild.content.firstChild.textContent, 'nested');
  check('template contents serialize', template.innerHTML, '<p>template</p><template><b>nested</b></template>');
  const mount = document.getElementById('mount');
  mount.innerHTML = template.outerHTML;
  check('template serialization round trips', mount.firstChild.content.lastChild.content.textContent, 'nested');
  check('imported template nodes belong to this document', mount.firstChild.content.firstChild.ownerDocument, document);
</script>"#);
    assert_all_ok(&lines, 13);
}

#[test]
fn outer_html_replaces_in_the_parent_context_and_keeps_detached_nodes() {
    let lines = run(
        r#"<div id=mount><i id=old>old</i><em>tail</em></div><script>
  const mount = document.getElementById('mount');
  const old = document.getElementById('old');
  old.outerHTML = '<b>one</b>middle<u>two</u>';
  check('outerHTML inserts all parsed siblings', mount.innerHTML, '<b>one</b>middle<u>two</u><em>tail</em>');
  check('outerHTML detaches the old node', old.parentNode, null);
  old.outerHTML = '<hr>';
  check('detached outerHTML setter is a no-op', old.outerHTML, '<i id="old">old</i>');
  throwsDom('the document root cannot be replaced', () => { document.documentElement.outerHTML = '<html></html>'; }, 'NoModificationAllowedError');
  const fragment = document.createDocumentFragment();
  const child = document.createElement('i');
  fragment.appendChild(child);
  child.outerHTML = '<p>fragment</p>';
  check('fragment parents use body context', fragment.firstChild instanceof HTMLParagraphElement, true);
  const table = document.createElement('table');
  table.innerHTML = '<tbody><tr><td>old</td></tr></tbody>';
  table.querySelector('td').outerHTML = '<td>one</td><td>two</td>';
  check('outerHTML uses the table row context', table.querySelectorAll('td').length, 2);
</script>"#,
    );
    assert_all_ok(&lines, 6);
}

#[test]
fn adjacent_html_inserts_in_order_without_replacing_existing_nodes() {
    let lines = run(r#"<div id=mount><i id=keep>keep</i></div><script>
  const mount = document.getElementById('mount');
  const keep = document.getElementById('keep');
  keep.saved = 7;
  keep.insertAdjacentHTML('beforebegin', '<b>before</b>');
  keep.insertAdjacentHTML('afterbegin', '<u>first</u>');
  keep.insertAdjacentHTML('beforeend', '<u>last</u>');
  keep.insertAdjacentHTML('afterend', '<b>after</b>');
  check('all four positions preserve order', mount.innerHTML, '<b>before</b><i id="keep"><u>first</u>keep<u>last</u></i><b>after</b>');
  check('existing wrapper identity survives insertion', document.getElementById('keep'), keep);
  check('existing expandos survive insertion', keep.saved, 7);
  keep.insertAdjacentHTML('BeFoReEnD', '!');
  check('positions are ASCII case-insensitive', keep.textContent, 'firstkeeplast!');
  throwsDom('invalid insertion position', () => keep.insertAdjacentHTML('nowhere', '<p>bad</p>'), 'SyntaxError');
  throwsDom('no parent to insert before', () => document.createElement('i').insertAdjacentHTML('beforebegin', '<p>bad</p>'), 'NoModificationAllowedError');
  throwsDom('no siblings beside the document root', () => document.documentElement.insertAdjacentHTML('afterend', '<p>bad</p>'), 'NoModificationAllowedError');
  const template = document.createElement('template');
  template.insertAdjacentHTML('beforeend', '<b>ordinary child</b>');
  check('adjacent HTML does not target template content', [template.childNodes.length, template.content.childNodes.length], [1, 0]);
</script>"#);
    assert_all_ok(&lines, 8);
}

#[test]
fn scripts_created_by_markup_setters_do_not_execute() {
    let lines = run(r#"<div id=mount></div><script>
  globalThis.ranInsertedScript = false;
  const mount = document.getElementById('mount');
  const code = '<scr' + 'ipt>globalThis.ranInsertedScript = true;</scr' + 'ipt>';
  mount.innerHTML = code;
  check('script markup remains in the DOM', mount.firstChild instanceof HTMLScriptElement, true);
  check('innerHTML does not execute inserted script', globalThis.ranInsertedScript, false);
  mount.insertAdjacentHTML('beforeend', code);
  check('adjacent HTML does not execute inserted script', globalThis.ranInsertedScript, false);
  mount.firstChild.outerHTML = code;
  check('outerHTML does not execute inserted script', globalThis.ranInsertedScript, false);
</script>"#);
    assert_all_ok(&lines, 4);
}

#[test]
fn markup_parsing_uses_the_documents_scripting_and_form_context() {
    let lines = run(r#"<form id=form><div id=inside></div></form><script>
  const noscript = document.createElement('noscript');
  noscript.innerHTML = '<b>literal markup</b>';
  check('noscript has raw text while scripting is enabled', [noscript.childElementCount, noscript.textContent], [0, '<b>literal markup</b>']);
  check('noscript serialization is literal', noscript.innerHTML, '<b>literal markup</b>');
  const inside = document.getElementById('inside');
  inside.innerHTML = '<form id=nested><input></form>';
  check('an ancestor form suppresses nested form tags', [inside.querySelector('form'), inside.firstChild.tagName], [null, 'INPUT']);
  inside.insertAdjacentHTML('beforeend', '<form id=another><button>button</button></form>');
  check('adjacent HTML preserves the form pointer', [inside.querySelectorAll('form').length, inside.lastChild.tagName], [0, 'BUTTON']);
  const form = document.createElement('form');
  form.innerHTML = '<form><input></form>';
  check('a form context also suppresses nested forms', form.querySelectorAll('form').length, 0);
</script>"#);
    assert_all_ok(&lines, 5);
}

#[test]
fn the_tree_is_walked_and_changed_as_the_dom_specifies() {
    let lines = run(
        r"<div id=list><p id=a>one</p><!--note--><p id=b>two</p></div>
<script>
  const list = document.getElementById('list');
  const [a, b] = [document.getElementById('a'), document.getElementById('b')];
  check('childNodes', list.childNodes.length, 3);
  check('childNodes is a NodeList', list.childNodes instanceof NodeList, true);
  check('firstChild', list.firstChild, a);
  check('nextSibling is the comment', a.nextSibling.nodeType, Node.COMMENT_NODE);
  check('previousSibling', b.previousSibling.data, 'note');
  check('parentNode of an element', a.parentNode, list);
  check('parentNode of html', document.documentElement.parentNode, document);
  check('parentElement of html', document.documentElement.parentElement, null);
  check('ownerDocument', a.ownerDocument, document);
  check('document has no ownerDocument', document.ownerDocument, null);
  check('contains itself', list.contains(list), true);
  check('contains a descendant', list.contains(a.firstChild), true);
  check('does not contain an ancestor', a.contains(list), false);

  const c = document.createElement('p');
  c.textContent = 'three';
  check('insertBefore returns the node', list.insertBefore(c, b), c);
  check('insertBefore placed it', b.previousSibling, c);
  check('insertBefore null appends', list.insertBefore(document.createElement('i'), null), list.lastChild);
  check('replaceChild returns the old child', list.replaceChild(document.createElement('hr'), c), c);
  check('replaced node is detached', c.parentNode, null);
  check('removeChild', list.removeChild(b), b);
  check('removed', b.isConnected, false);

  const shallow = a.cloneNode();
  const deep = a.cloneNode(true);
  check('shallow clone has no children', shallow.childNodes.length, 0);
  check('deep clone has the text', deep.textContent, 'one');
  check('clone keeps attributes', deep.id, 'a');
  check('clone is detached', deep.parentNode, null);
  check('clone is equal', deep.isEqualNode(a), true);
  check('clone is not the same', deep.isSameNode(a), false);

  check('document textContent is null', document.textContent, null);
  check('comment textContent', list.childNodes[1].textContent, 'note');
  check('nodeValue of an element', a.nodeValue, null);
  check('compare: a follows html', document.documentElement.compareDocumentPosition(a) & Node.DOCUMENT_POSITION_FOLLOWING, Node.DOCUMENT_POSITION_FOLLOWING);
  check('compare: list contains a', a.compareDocumentPosition(list), Node.DOCUMENT_POSITION_CONTAINS | Node.DOCUMENT_POSITION_PRECEDING);

  const holder = document.createElement('div');
  holder.append('x', 'y', document.createTextNode('z'));
  check('three text nodes', holder.childNodes.length, 3);
  holder.normalize();
  check('normalize merges them', [holder.childNodes.length, holder.textContent], [1, 'xyz']);

  throwsDom('a node into its own child', () => a.appendChild(list), 'HierarchyRequestError');
  throwsDom('before a stranger', () => list.insertBefore(document.createElement('b'), holder), 'NotFoundError');
  throwsDom('removing a stranger', () => list.removeChild(holder), 'NotFoundError');
  throwsDom('text into the document', () => document.appendChild(document.createTextNode('x')), 'HierarchyRequestError');
  throwsDom('a second root element', () => document.appendChild(document.createElement('html')), 'HierarchyRequestError');
  check('getRootNode of a detached node', holder.firstChild.getRootNode(), holder);
</script>",
    );
    assert_all_ok(&lines, 39);
}

#[test]
fn every_element_is_made_as_its_own_interface() {
    let lines = run(
        r"<a id=link href=x>x</a><svg id=icon viewBox='0 0 10 10'><path d='M0 0'/></svg><template id=t><b>bold</b></template>
<script>
  const div = document.createElement('div');
  check('div is an HTMLDivElement', div instanceof HTMLDivElement, true);
  check('and an HTMLElement', div instanceof HTMLElement, true);
  check('and an Element', div instanceof Element, true);
  check('and a Node', div instanceof Node, true);
  check('and an EventTarget', div instanceof EventTarget, true);
  check('div tagName', div.tagName, 'DIV');
  check('toString tag', Object.prototype.toString.call(div), '[object HTMLDivElement]');
  check('a parsed link', document.getElementById('link') instanceof HTMLAnchorElement, true);
  const svg = document.getElementById('icon');
  check('parsed svg', svg instanceof SVGSVGElement, true);
  check('svg is an SVGElement', svg instanceof SVGElement, true);
  check('svg tagName keeps its case', svg.tagName, 'svg');
  check('path', svg.firstChild instanceof SVGPathElement, true);
  check('path is geometry', svg.firstChild instanceof SVGGeometryElement, true);
  const made = document.createElementNS('http://www.w3.org/2000/svg', 'circle');
  check('createElementNS svg', [made instanceof SVGCircleElement, made.namespaceURI], [true, 'http://www.w3.org/2000/svg']);
  check('unknown element', document.createElement('blink') instanceof HTMLUnknownElement, true);
  check('custom element name', document.createElement('my-widget').constructor, HTMLElement);
  check('document', document instanceof HTMLDocument, true);
  check('document is a Document', document instanceof Document, true);
  const text = new Text('made');
  check('new Text', [text instanceof Text, text.data], [true, 'made']);
  document.body.appendChild(text);
  check('new Text keeps its identity', document.body.lastChild, text);
  check('new DocumentFragment', new DocumentFragment() instanceof DocumentFragment, true);
  check('new Comment', new Comment('c').nodeType, 8);
  const template = document.getElementById('t');
  check('template content', template.content instanceof DocumentFragment, true);
  check('template content holds the markup', template.content.firstChild.tagName, 'B');
  check('template has no children', template.childNodes.length, 0);
  check('content is HTMLTemplateElement only', 'content' in div, false);
  check('Node constants', [Node.ELEMENT_NODE, div.TEXT_NODE], [1, 3]);
  check('illegal constructor', (() => { try { new HTMLDivElement(); return 'made'; } catch (e) { return e instanceof TypeError; } })(), true);
  check('one wrapper per node', document.body.firstElementChild === document.querySelector('a'), true);
  check('expando survives lookups', (() => { div.id = 'exp'; document.body.appendChild(div); div.__mine = 7; return document.getElementById('exp').__mine; })(), 7);
  check('Image', [new Image(4, 5) instanceof HTMLImageElement, new Image(4, 5).width], [true, 4]);
  check('Option', [new Option('Label', 'v').value, new Option('Label').text], ['v', 'Label']);
</script>",
    );
    assert_all_ok(&lines, 32);
}

#[test]
fn attributes_keep_their_case_where_the_namespace_says_so() {
    let lines = run(r#"<div id=d data-user-id=42 class="a b"></div>
<script>
  const d = document.getElementById('d');
  d.setAttribute('DATA-X', '1');
  check('HTML attribute names are lowercased', d.getAttribute('data-x'), '1');
  check('and read case-insensitively', d.getAttribute('Data-X'), '1');
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  check('SVG keeps viewBox', svg.getAttributeNames(), ['viewBox']);
  check('SVG reads it back', svg.getAttribute('viewBox'), '0 0 24 24');
  const use = document.createElementNS('http://www.w3.org/2000/svg', 'use');
  use.setAttributeNS('http://www.w3.org/1999/xlink', 'xlink:href', '#icon');
  check('xlink:href by namespace', use.getAttributeNS('http://www.w3.org/1999/xlink', 'href'), '#icon');
  check('xlink:href by qualified name', use.getAttribute('xlink:href'), '#icon');
  check('plain href is a different attribute', use.getAttribute('href'), null);
  check('toggleAttribute on', d.toggleAttribute('hidden'), true);
  check('toggleAttribute off', [d.toggleAttribute('hidden'), d.hasAttribute('hidden')], [false, false]);
  check('toggleAttribute forced', d.toggleAttribute('hidden', true), true);
  const attrs = d.attributes;
  check('attributes length', attrs.length, d.getAttributeNames().length);
  check('attributes by name', attrs.getNamedItem('class').value, 'a b');
  check('attributes iterate', [...attrs].map((a) => a.name).includes('data-user-id'), true);
  check('dataset reads', d.dataset.userId, '42');
  d.dataset.fooBar = 'baz';
  check('dataset writes', d.getAttribute('data-foo-bar'), 'baz');
  check('dataset keys', Object.keys(d.dataset).sort(), ['fooBar', 'userId', 'x']);
  delete d.dataset.fooBar;
  check('dataset deletes', d.hasAttribute('data-foo-bar'), false);
  check('classList is one object', d.classList === d.classList, true);
  d.classList.add('c');
  d.classList.remove('a');
  check('classList edits the attribute', d.className, 'b c');
  check('classList toggle', [d.classList.toggle('b'), d.classList.contains('b')], [false, false]);
  throwsDom('an empty token', () => d.classList.add(''), 'SyntaxError');
  throwsDom('a bad attribute name', () => d.setAttribute('a b', 'x'), 'InvalidCharacterError');
  check('style is one object', d.style === d.style, true);
  d.style.setProperty('--Brand', 'red');
  check('custom properties keep their case', d.style.getPropertyValue('--Brand'), 'red');
  d.style.backgroundColor = 'blue';
  check('style writes the attribute', d.getAttribute('style').includes('background-color: blue'), true);
</script>"#);
    assert_all_ok(&lines, 25);
}

#[test]
fn the_variadic_mixins_take_nodes_and_strings() {
    let lines = run(r"<ul id=list><li id=mid>mid</li></ul>
<script>
  const list = document.getElementById('list');
  const mid = document.getElementById('mid');
  const text = () => list.textContent;
  list.prepend('a', document.createElement('br'));
  check('prepend', [list.firstChild.data, list.childNodes[1].tagName], ['a', 'BR']);
  list.append('z');
  check('append', list.lastChild.data, 'z');
  mid.before('<');
  mid.after('>');
  check('before and after', text(), 'a<mid>z');
  mid.replaceWith('MID');
  check('replaceWith', [text(), mid.parentNode], ['a<MID>z', null]);
  list.replaceChildren('only');
  check('replaceChildren', [list.childNodes.length, text()], [1, 'only']);
  const item = document.createElement('li');
  check('insertAdjacentElement', list.insertAdjacentElement('afterbegin', item), item);
  list.insertAdjacentText('beforeend', '!');
  check('insertAdjacentText', text(), 'only!');
  check('nowhere to go before a detached node', document.createElement('p').insertAdjacentElement('beforebegin', item), null);
  throwsDom('a bad position', () => list.insertAdjacentText('nowhere', 'x'), 'SyntaxError');
  const comment = document.createComment('gone');
  list.appendChild(comment);
  comment.remove();
  check('a comment removes itself', comment.parentNode, null);
</script>");
    assert_all_ok(&lines, 10);
}

#[test]
fn selectors_match_in_scope_and_bad_ones_throw() {
    let lines = run(
        r"<section id=s><div class=card><p class=x>1</p></div><div class=card><p class=x>2</p></div></section>
<script>
  const s = document.getElementById('s');
  const p = s.querySelector('p');
  check('matches', p.matches('.card > .x'), true);
  check('webkitMatchesSelector', p.webkitMatchesSelector('section p'), true);
  check('closest', p.closest('.card') === s.firstElementChild, true);
  check('closest includes itself', p.closest('p'), p);
  check('closest misses', p.closest('table'), null);
  check(':scope', s.querySelectorAll(':scope > div').length, 2);
  check('querySelectorAll is a NodeList', s.querySelectorAll('p') instanceof NodeList, true);
  check('in tree order', [...s.querySelectorAll('.x')].map((e) => e.textContent), ['1', '2']);
  const fragment = document.createDocumentFragment();
  fragment.appendChild(s.cloneNode(true));
  check('a fragment can be queried', fragment.querySelectorAll('.card').length, 2);
  check('getElementById on a fragment', fragment.getElementById('s') !== null, true);
  check('children is a collection', s.children instanceof HTMLCollection, true);
  check('namedItem', document.getElementsByTagName('section').namedItem('s'), s);
  check('getElementsByClassName', document.getElementsByClassName('card x').length, 0);
  check('getElementsByTagName finds SVG names too', document.getElementsByTagName('DIV').length, 2);
  throwsDom('a bad selector', () => document.querySelector('p['), 'SyntaxError');
  throwsDom('a bad selector to matches', () => p.matches(':::'), 'SyntaxError');
</script>",
    );
    assert_all_ok(&lines, 16);
}

#[test]
fn reflected_attributes_read_as_the_platform_reads_them() {
    let lines = run(
        r"<a id=a href='../other/x?q=1#h'>link</a><form id=f><input id=i name=n disabled><td></td></form><table><tr><td id=c>c</td></tr></table>
<script>
  const a = document.getElementById('a');
  check('href resolves', a.href, 'https://example.com/other/x?q=1#h');
  check('pathname', a.pathname, '/other/x');
  check('search', a.search, '?q=1');
  check('hash', a.hash, '#h');
  a.hash = 'there';
  check('setting a part rewrites href', a.getAttribute('href'), 'https://example.com/other/x?q=1#there');
  check('origin', a.origin, 'https://example.com');
  const input = document.getElementById('i');
  check('disabled is a boolean', input.disabled, true);
  input.disabled = false;
  check('setting it removes the attribute', input.hasAttribute('disabled'), false);
  check('type defaults to text', input.type, 'text');
  check('form owner', input.form, document.getElementById('f'));
  check('form method default', document.getElementById('f').method, 'get');
  check('form action defaults to the page', document.getElementById('f').action, 'https://example.com/dir/page.html');
  check('form elements', document.getElementById('f').elements.length, 1);
  check('colSpan default', document.getElementById('c').colSpan, 1);
  check('cells', document.querySelector('tr').cells.length, 1);
  check('tabIndex of a link', a.tabIndex, 0);
  check('tabIndex of a div', document.createElement('div').tabIndex, -1);
  check('hidden reflects', (() => { const d = document.createElement('div'); d.hidden = true; return d.getAttribute('hidden'); })(), '');
  check('dir enumerates', (() => { const d = document.createElement('div'); d.setAttribute('dir', 'RTL'); return d.dir; })(), 'rtl');
  check('baseURI', document.body.baseURI, 'https://example.com/dir/page.html');
</script>",
    );
    assert_all_ok(&lines, 20);
}

#[test]
fn character_data_counts_in_utf16() {
    let lines = run(r"<p id=p>héllo 😀 world</p>
<script>
  const text = document.getElementById('p').firstChild;
  check('length in code units', text.length, 14);
  check('substringData', text.substringData(6, 2), '😀');
  const tail = text.splitText(9);
  check('splitText keeps the head', text.data, 'héllo 😀 ');
  check('and makes the tail', tail.data, 'world');
  check('tail follows', text.nextSibling, tail);
  check('wholeText', tail.wholeText, 'héllo 😀 world');
  tail.appendData('!');
  tail.insertData(0, '>');
  tail.deleteData(1, 1);
  check('edits', tail.data, '>orld!');
  tail.replaceData(0, 1, '');
  check('replaceData', tail.data, 'orld!');
  throwsDom('past the end', () => tail.substringData(99, 1), 'IndexSizeError');
  tail.data = 'new';
  check('data setter', document.getElementById('p').textContent, 'héllo 😀 new');
</script>");
    assert_all_ok(&lines, 10);
}

#[test]
fn legacy_create_event_and_init_methods_build_dispatchable_events() {
    let lines = run(r#"<div id=target></div><script>
  const target = document.getElementById('target');
  const plain = document.createEvent('Event');
  plain.initEvent('foo', true, true);
  plain.preventDefault();
  check('initEvent sets type and flags', [plain.type, plain.bubbles, plain.cancelable, plain.defaultPrevented], ['foo', true, true, true]);
  const custom = document.createEvent('CustomEvent');
  custom.initCustomEvent('bar', false, false, { n: 1 });
  let heard = null;
  target.addEventListener('bar', (event) => { heard = event.detail.n; });
  target.dispatchEvent(custom);
  check('a custom event carries its detail', heard, 1);
  const mouse = document.createEvent('MouseEvents');
  mouse.initMouseEvent('click', true, true, window, 1, 0, 0, 7, 9, false, false, true, false, 0, null);
  check('mouse fields', [mouse instanceof MouseEvent, mouse.clientX, mouse.clientY, mouse.shiftKey, mouse.detail], [true, 7, 9, true, 1]);
  check('HTMLEvents is Event', document.createEvent('HTMLEvents') instanceof Event, true);
  let refused;
  try { document.createEvent('TouchEvent'); } catch (error) { refused = error.name; }
  check('TouchEvent is refused', refused, 'NotSupportedError');
</script>"#);
    assert_all_ok(&lines, 5);
}

#[test]
fn every_event_target_shares_one_listener_implementation() {
    let lines = run(r#"<div id=target></div><script>
  check('the window is an EventTarget', [window instanceof Window, window instanceof EventTarget, Object.getPrototypeOf(window) === Window.prototype], [true, true, true]);
  const copied = Object.getOwnPropertyDescriptor(EventTarget.prototype, 'addEventListener').value;
  let windowHeard = 0;
  copied.call(window, 'ping', () => { windowHeard++; });
  window.dispatchEvent(new Event('ping'));
  check('a method copied from EventTarget.prototype works on the window', windowHeard, 1);
  class Bus extends EventTarget {}
  const bus = new Bus();
  let busHeard = 0;
  bus.addEventListener('ping', () => { busHeard++; });
  bus.dispatchEvent(new Event('ping', { bubbles: true }));
  check('a page-made target hears its event and the window does not', [busHeard, windowHeard], [1, 1]);
  const detached = document.createElement('span');
  detached.dispatchEvent(new Event('ping', { bubbles: true }));
  check('a detached node does not reach the window', windowHeard, 1);
  document.getElementById('target').dispatchEvent(new Event('ping', { bubbles: true }));
  check('a connected node does', windowHeard, 2);
  const controller = new AbortController();
  let aborted = false;
  controller.signal.addEventListener('abort', () => { aborted = true; });
  controller.abort();
  check('engine-made targets still fire', [aborted, controller.signal.aborted], [true, true]);
  check('the global still has Object methods', [typeof window.hasOwnProperty, String(window) === '[object Window]' || String(window).length > 0], ['function', true]);
</script>"#);
    assert_all_ok(&lines, 7);
}

#[test]
fn cloning_a_template_clones_its_contents() {
    let lines = run(r#"<script>
  const outer = document.createElement('template');
  outer.innerHTML = '<div><template><b>x</b><i>y</i></template></div>';
  const stamped = document.importNode(outer.content, true);
  check('importNode copies a nested template', stamped.querySelector('template').content.childNodes.length, 2);
  const copy = outer.content.cloneNode(true);
  check('cloneNode copies a nested template', copy.querySelector('template').content.textContent, 'xy');
  const element = outer.cloneNode(true);
  check('a cloned template has the contents', element.content.firstChild.localName, 'div');
  check('the copy is its own', element.content === outer.content, false);
  const shallow = outer.cloneNode(false);
  check('a shallow clone has empty contents', shallow.content.childNodes.length, 0);
</script>"#);
    assert_all_ok(&lines, 5);
}

#[test]
fn polymer_template_parsing_steps() {
    let lines = run(r#"<script>
  const t = document.createElement('template');
  t.innerHTML = '<div><template is="dom-if" if="[[show]]"><b>x [[title]]</b></template></div>';
  const inner = t.content.querySelector('template');
  const wrapper = document.createElement('dom-if');
  inner.parentNode.replaceChild(wrapper, inner);
  wrapper.appendChild(inner);
  inner.removeAttribute('is');
  while (inner.attributes.length) { wrapper.setAttribute(inner.attributes[0].name, inner.attributes[0].value); inner.removeAttribute(inner.attributes[0].name); }
  check('attributes moved', [wrapper.getAttribute('if'), inner.attributes.length], ['[[show]]', 0]);
  wrapper.removeAttribute('if');
  const content = inner.content.ownerDocument.createDocumentFragment();
  content.appendChild(inner.content);
  check('content moved', [content.childNodes.length, inner.content.childNodes.length], [1, 0]);
  const stamped = document.importNode(t.content, true);
  const w2 = stamped.querySelector('dom-if');
  check('stamped wrapper', [!!w2, w2 && w2.hasAttribute('if'), w2 && w2.firstElementChild && w2.firstElementChild.localName], [true, false, 'template']);
  const b = content.firstChild;
  check('text node data', b.firstChild.data, 'x [[title]]');
  b.firstChild.textContent = '';
  check('text cleared', b.textContent, '');
</script>"#);
    assert_all_ok(&lines, 5);
}

#[test]
fn mutation_observers_hear_what_they_asked_for() {
    let lines = run(r#"<div id=root><p id=a title=t>text</p></div><script>
  const root = document.getElementById('root');
  const a = document.getElementById('a');
  const seen = [];
  const all = new MutationObserver((records, observer) => {
    for (const r of records) seen.push(r.type + ':' + (r.attributeName || '') + ':' + r.oldValue + ':' + r.addedNodes.length + '/' + r.removedNodes.length + ':' + (r.target.id || r.target.nodeName));
    check('the observer is passed itself', observer, all);
  });
  all.observe(root, { childList: true, subtree: true, attributes: true, attributeOldValue: true, characterData: true, characterDataOldValue: true });
  const childOnly = new MutationObserver((records) => { seen.push('child-only ' + records.length); });
  childOnly.observe(root, { childList: true });
  a.title = 'u';
  a.firstChild.data = 'changed';
  const b = document.createElement('b');
  a.appendChild(b);
  root.appendChild(document.createElement('i'));
  a.removeChild(b);
  const filtered = new MutationObserver((records) => { seen.push('filtered ' + records.map((r) => r.attributeName).join()); });
  filtered.observe(a, { attributeFilter: ['data-x'] });
  a.setAttribute('data-x', '1');
  a.setAttribute('data-y', '2');
  check('nothing is delivered before the checkpoint', seen.length, 0);
  const taken = new MutationObserver(() => seen.push('never'));
  taken.observe(a, { attributes: true });
  a.id = 'a';
  check('takeRecords empties the queue', taken.takeRecords().length, 1);
  let threw = '';
  try { new MutationObserver(() => {}).observe(a, {}); } catch (e) { threw = e.name; }
  check('options must ask for something', threw, 'TypeError');
  setTimeout(() => {
    check('records arrive in order', seen, [
      'attributes:title:t:0/0:a',
      'characterData::text:0/0:#text',
      'childList::null:1/0:a',
      'childList::null:1/0:root',
      'childList::null:0/1:a',
      'attributes:data-x:null:0/0:a',
      'attributes:data-y:null:0/0:a',
      'attributes:id:a:0/0:a',
      'child-only 1',
      'filtered data-x',
    ]);
    // Polymer's microtask: a callback after a change to a watched text node.
    const node = document.createTextNode('');
    let flushed = 0;
    new MutationObserver(() => { flushed++; }).observe(node, { characterData: true });
    node.textContent = '1';
    setTimeout(() => check('a text node watched as a microtask flushes', flushed, 1), 0);
  }, 0);
</script>"#);
    assert_all_ok(&lines, 6);
}

#[test]
fn a_style_element_has_a_sheet_script_can_change() {
    let lines = run(
        r#"<style id=s>p { color: red } div { margin: 0 }</style><script>
  const style = document.getElementById('s');
  const sheet = style.sheet;
  check('a style element has a sheet', [sheet instanceof CSSStyleSheet, sheet.ownerNode === style, sheet.cssRules.length], [true, true, 2]);
  check('the same sheet each time', style.sheet === sheet, true);
  check('document.styleSheets lists it', [document.styleSheets.length >= 1, Array.from(document.styleSheets).includes(sheet)], [true, true]);
  sheet.insertRule('.made { color: blue }', sheet.cssRules.length);
  sheet.insertRule('.first { color: green }', 0);
  check('inserted rules are in place', [sheet.cssRules.length, sheet.cssRules[0].cssText, sheet.cssRules[0].selectorText], [4, '.first { color: green }', '.first']);
  sheet.deleteRule(1);
  check('a deleted rule is gone', sheet.cssRules.length, 3);
  let threw = '';
  try { sheet.insertRule('.x {}', 99); } catch (e) { threw = e.name; }
  check('an index past the end is refused', threw, 'IndexSizeError');
  const made = document.createElement('style');
  document.head.appendChild(made);
  made.appendChild(document.createTextNode(''));
  check('a styled-components style element gets a sheet', made.sheet.cssRules.length, 0);
</script>"#,
    );
    assert_all_ok(&lines, 7);
}

#[test]
fn scripts_script_inserts_run_once_and_markup_ones_do_not() {
    let lines = run(
        r#"<div id=host></div><script id=parsed>window.parsedRuns = (window.parsedRuns || 0) + 1;</script><script>
  const host = document.getElementById('host');
  const inline = document.createElement('script');
  inline.textContent = 'window.inlineRan = document.currentScript === inlineScript;';
  window.inlineScript = inline;
  host.appendChild(inline);
  check('an inserted inline script runs at once, as the current script', window.inlineRan, true);
  check('the current script is cleared after', document.currentScript && document.currentScript.id, null);
  host.innerHTML = '<script>window.fromMarkup = true<\/script>';
  check('markup does not run what it writes', window.fromMarkup, undefined);
  host.appendChild(document.getElementById('parsed'));
  check('moving a parsed script does not run it again', window.parsedRuns, 1);
  const frag = document.createDocumentFragment();
  const inFragment = document.createElement('script');
  inFragment.text = 'window.fragmentRan = (window.fragmentRan || 0) + 1';
  frag.appendChild(inFragment);
  check('a script in a detached fragment waits', window.fragmentRan, undefined);
  host.append(frag);
  host.appendChild(inFragment);
  check('it runs when connected, once', window.fragmentRan, 1);
  const data = document.createElement('script');
  data.type = 'text/template';
  data.text = 'window.templateRan = true';
  host.appendChild(data);
  check('a script of another type is data', window.templateRan, undefined);
  const broken = document.createElement('script');
  broken.text = 'throw new Error("inner")';
  let reported = '';
  window.onerror = (message) => { reported = message; return true; };
  host.appendChild(broken);
  check('its error is reported like a script error', reported, 'Uncaught Error: inner');
</script>"#,
    );
    assert_all_ok(&lines, 8);
}

#[test]
fn user_timing_marks_and_measures() {
    let lines = run(r#"<script>
  const a = performance.mark('a');
  performance.mark('b', { startTime: a.startTime + 5 });
  const m = performance.measure('a-b', 'a', 'b');
  // (t + 5) - t is 5 to within a rounding of t, not exactly.
  check('a measure spans its marks', [m.entryType, Math.round(m.duration * 1e6) / 1e6], ['measure', 5]);
  check('entries are listed', [performance.getEntriesByType('mark').length, performance.getEntriesByName('a-b').length], [2, 1]);
  performance.clearMarks('a');
  performance.clearMeasures();
  check('cleared', [performance.getEntriesByType('mark').map((e) => e.name), performance.getEntriesByType('measure').length], [['b'], 0]);
</script>"#);
    assert_all_ok(&lines, 3);
}

#[test]
fn a_page_does_not_look_like_node() {
    let lines = run(r#"<script>
  check('no process', typeof process, 'undefined');
  check('no require, module or Buffer', [typeof require, typeof module, typeof Buffer], ['undefined', 'undefined', 'undefined']);
  const isNode = typeof process === 'object' && !!process.versions && !!process.versions.node;
  check('the Node test answers no', isNode, false);
</script>"#);
    assert_all_ok(&lines, 3);
}

#[test]
fn attribute_nodes_are_removed_and_path2d_is_there() {
    let lines = run(r#"<script>
  const el = document.createElement('div');
  el.setAttribute('data-x', '1');
  const attr = el.getAttributeNode('data-x');
  check('removeAttributeNode answers the node', el.removeAttributeNode(attr), attr);
  check('the attribute is gone and the node keeps its value', [el.hasAttribute('data-x'), attr.value, attr.ownerElement], [false, '1', null]);
  let name = '';
  try { el.removeAttributeNode(attr); } catch (error) { name = error.name; }
  check('removing it again is NotFoundError', name, 'NotFoundError');
  const path = new Path2D();
  path.moveTo(0, 0);
  path.roundRect(0, 0, 10, 10, 2);
  check('Path2D is a path', [path instanceof Path2D, typeof Path2D.prototype.arc, typeof new Path2D(path).addPath], [true, 'function', 'function']);
</script>"#);
    assert_all_ok(&lines, 4);
}

#[test]
fn a_2d_canvas_draws_into_pixels_script_can_read() {
    let lines = run(r#"<canvas id=c width=40 height=20></canvas><script>
  const canvas = document.getElementById('c');
  const ctx = canvas.getContext('2d');
  check('the same context each time', [ctx === canvas.getContext('2d'), canvas.getContext('webgl')], [true, null]);
  ctx.fillStyle = 'rgb(255, 0, 0)';
  ctx.fillRect(0, 0, 10, 10);
  check('a filled rectangle is in the pixels', Array.from(ctx.getImageData(5, 5, 1, 1).data), [255, 0, 0, 255]);
  check('outside it is transparent', Array.from(ctx.getImageData(30, 15, 1, 1).data), [0, 0, 0, 0]);
  ctx.fillStyle = 'nonsense';
  check('a colour that does not parse is ignored', ctx.fillStyle, '#ff0000');
  ctx.translate(20, 0);
  const path = new Path2D();
  path.rect(0, 0, 5, 5);
  ctx.fillStyle = '#00f';
  ctx.fill(path);
  check('a Path2D is drawn through the transform', Array.from(ctx.getImageData(22, 2, 1, 1).data), [0, 0, 255, 255]);
  check('isPointInPath reads the transformed path', ctx.isPointInPath(path, 22, 2), true);
  const image = ctx.createImageData(1, 1);
  image.data.set([1, 2, 3, 255]);
  ctx.putImageData(image, 39, 19);
  check('putImageData writes straight pixels', Array.from(ctx.getImageData(39, 19, 1, 1).data), [1, 2, 3, 255]);
  ctx.font = 'bold 12px sans-serif';
  check('font reads back and text measures', [ctx.font, ctx.measureText('abc').width > 10], ['bold 12px sans-serif', true]);
  check('toDataURL is a PNG', canvas.toDataURL().startsWith('data:image/png;base64,iVBOR'), true);
  canvas.width = 40;
  check('setting the width clears it', Array.from(ctx.getImageData(5, 5, 1, 1).data), [0, 0, 0, 0]);
</script>"#);
    assert_all_ok(&lines, 10);
}

#[test]
fn markup_set_from_script_keeps_a_leading_zero_width_no_break_space() {
    let lines = run(r#"<script>
  const div = document.createElement('div');
  div.innerHTML = '\ufeffIssues';
  const parsed = new DOMParser().parseFromString('\ufeffIssues', 'text/html');
  check('innerHTML keeps it', div.textContent, '\ufeffIssues');
  check('DOMParser keeps it', parsed.body.textContent, '\ufeffIssues');
</script>"#);
    assert_all_ok(&lines, 2);
}

#[test]
fn script_selectors_take_has() {
    let lines = run(
        r#"<ul id=list><li class=a><b>x</b></li><li class=b></li></ul><script>
  check('querySelector with :has', document.querySelector('li:has(> b)').className, 'a');
  check('matches with :has', [document.getElementById('list').matches(':has(.b)'), document.querySelector('.b').matches(':has(*)')], [true, false]);
  check('closest with :has', document.querySelector('b').closest('ul:has(li.b)').id, 'list');
  check('nested in :is and :not', document.querySelectorAll('li:not(:has(b))').length, 1);
</script>"#,
    );
    assert_all_ok(&lines, 4);
}

#[test]
fn object_urls_name_blobs_for_fetch() {
    let lines = run(r#"<script>
  const blob = new Blob(['hello'], { type: 'text/plain' });
  const url = URL.createObjectURL(blob);
  check('an object URL is a blob: address of this origin', url.startsWith('blob:'), true);
  fetch(url).then((response) => response.text()).then((text) => {
    check('fetch reads the blob', [text, URL.createObjectURL(blob) !== url], ['hello', true]);
    URL.revokeObjectURL(url);
    return fetch(url).then(() => 'fetched', () => 'refused');
  }).then((after) => check('a revoked URL is refused', after, 'refused'));
</script>"#);
    assert_all_ok(&lines, 3);
}

#[test]
fn container_properties_compute_to_their_initial_values() {
    let lines = run(r#"<div id=d></div><script>
  const style = getComputedStyle(document.getElementById('d'));
  check('container-type and -name are there', [style.containerType, style.containerName, style.getPropertyValue('container')], ['normal', 'none', 'none']);
  check('a page splitting the name can', style.containerName.split(' ').includes('homepage'), false);
</script>"#);
    assert_all_ok(&lines, 2);
}

#[test]
fn an_error_event_carries_the_exception() {
    let lines = run(r#"<div id=host></div><script>
  let seen = null;
  window.addEventListener('error', (event) => { seen = event; event.preventDefault(); });
  const broken = document.createElement('script');
  broken.text = 'throw new TypeError("from an inserted script")';
  document.getElementById('host').appendChild(broken);
  check('event.error is the exception', [seen && seen.error instanceof TypeError, seen && seen.error.message], [true, 'from an inserted script']);
  check('event.message names it', seen && seen.message, 'Uncaught TypeError: from an inserted script');
</script>"#);
    assert_all_ok(&lines, 2);
}

#[test]
fn indexed_db_keeps_records_for_the_document() {
    let lines = run(r#"<script>
  const promised = (request) => new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  const open = indexedDB.open('app', 2);
  let upgraded = null;
  open.onupgradeneeded = (event) => {
    upgraded = [event.oldVersion, event.newVersion];
    const db = open.result;
    const notes = db.createObjectStore('notes', { keyPath: 'id', autoIncrement: true });
    notes.createIndex('byTag', 'tags', { multiEntry: true });
    db.createObjectStore('kv');
  };
  promised(open).then(async (db) => {
    check('the upgrade ran from nothing to 2', [upgraded, db.version, [...db.objectStoreNames]], [[0, 2], 2, ['kv', 'notes']]);
    const write = db.transaction(['notes', 'kv'], 'readwrite');
    const notes = write.objectStore('notes');
    const first = await promised(notes.add({ text: 'a', tags: ['x', 'y'] }));
    const second = await promised(notes.put({ text: 'b', tags: ['y'] }));
    await promised(write.objectStore('kv').put({ nested: [1, 2] }, 'settings'));
    check('keys come from the generator', [first, second], [1, 2]);
    await new Promise((resolve) => { write.oncomplete = resolve; });
    const read = db.transaction('notes');
    const all = await promised(read.objectStore('notes').getAll());
    check('records come back in key order with their keys put in', all.map((n) => [n.id, n.text]), [[1, 'a'], [2, 'b']]);
    const tagged = await promised(read.objectStore('notes').index('byTag').getAll('y'));
    check('a multi-entry index finds both', tagged.map((n) => n.text), ['a', 'b']);
    const range = await promised(read.objectStore('notes').getAllKeys(IDBKeyRange.lowerBound(1, true)));
    check('a key range bounds the keys', range, [2]);
    const seen = [];
    await new Promise((resolve) => {
      const cursor = read.objectStore('notes').openCursor(null, 'prev');
      cursor.onsuccess = () => {
        if (!cursor.result) return resolve();
        seen.push(cursor.result.value.text);
        cursor.result.continue();
      };
    });
    check('a cursor walks backwards', seen, ['b', 'a']);
    const kv = await promised(db.transaction('kv').objectStore('kv').get('settings'));
    check('values are structured clones', kv, { nested: [1, 2] });
    const failing = db.transaction('notes', 'readwrite');
    failing.objectStore('notes').put({ id: 1, text: 'changed', tags: [] });
    const duplicate = failing.objectStore('notes').add({ id: 2, text: 'dup', tags: [] });
    const errorName = await new Promise((resolve) => { duplicate.onerror = () => resolve(duplicate.error.name); });
    await new Promise((resolve) => { failing.onabort = resolve; });
    const after = await promised(db.transaction('notes').objectStore('notes').get(1));
    check('an unhandled error aborts and the write is undone', [errorName, after.text], ['ConstraintError', 'a']);
    check('cmp orders keys by type', [indexedDB.cmp(1, 'a'), indexedDB.cmp(['a'], 'a'), indexedDB.cmp(2, 2)], [-1, 1, 0]);
  }).catch((error) => check('no failure', String(error), ''));
</script>"#);
    assert_all_ok(&lines, 9);
}

#[test]
fn the_interfaces_the_sweep_found_missing() {
    let lines = run(r#"<div id=d><b>bold</b> &amp; text</div><script>
  const serialized = new XMLSerializer().serializeToString(document.getElementById('d'));
  check('XMLSerializer writes markup with the XHTML namespace', serialized, '<div xmlns="http://www.w3.org/1999/xhtml" id="d"><b>bold</b> &amp; text</div>');
  window.addEventListener('message', (event) => {
    check('postMessage reaches the window as a clone', [event.data, event.origin, event.source === window], [{ a: [1] }, 'https://example.com', true]);
  });
  postMessage({ a: [1] }, '*');
  postMessage('dropped', 'https://elsewhere.test');
  const reader = new FileReader();
  reader.onload = () => {
    check('FileReader reads a data URL', reader.result, 'data:text/plain;base64,aGk=');
  };
  reader.readAsDataURL(new Blob(['hi'], { type: 'text/plain' }));
  const socket = new WebSocket('wss://example.com/live');
  const seen = [];
  socket.onerror = () => seen.push('error');
  socket.onclose = (event) => {
    seen.push('close ' + event.code);
    check('a socket fails like an unreachable server', [seen, socket.readyState, socket.url], [['error', 'close 1006'], 3, 'wss://example.com/live']);
  };
  let invalid = '';
  try { new WebSocket('ftp://x'); } catch (error) { invalid = error.name; }
  check('a socket to another scheme is refused at once', invalid, 'SyntaxError');
  check('hit testing outside the window finds nothing', document.elementFromPoint(-1, -1), null);
  check('StorageEvent and FontFace exist', [new StorageEvent('storage', { key: 'k' }).key, new FontFace('X', 'url(x.woff)').family], ['k', 'X']);
  const probe = document.createElement('div');
  probe.setAttribute('onfocusin', 't');
  probe.setAttribute('length', 'x');
  check('attributes answer by name', [probe.attributes.onfocusin.value, probe.attributes['onfocusin'].expando, probe.attributes.length], ['t', undefined, 2]);
</script>"#);
    assert_all_ok(&lines, 8);
}

#[test]
fn storage_quota_is_a_profiles_not_a_private_windows() {
    let lines = run(r#"<script>
  const descriptor = Object.getOwnPropertyDescriptor(Navigator.prototype, 'webkitTemporaryStorage');
  check('the old quota interface is a Navigator attribute', typeof descriptor?.get, 'function');
  navigator.webkitTemporaryStorage.queryUsageAndQuota((usage, quota) => {
    check('its quota is more than twice a heap limit', [usage, quota > 2 * 2 ** 30], [0, true]);
  }, () => console.log('FAIL the quota query failed'));
  navigator.storage.estimate().then(({ quota }) => {
    check('the estimate gives the same quota', quota > 2 * 2 ** 30, true);
  });
</script>"#);
    assert_all_ok(&lines, 3);
}

#[test]
fn attributes_without_state_are_still_accessors() {
    let lines = run(r#"<script>
  const pairs = [[HTMLMediaElement, 'paused'], [HTMLMediaElement, 'currentTime'], [HTMLVideoElement, 'videoWidth'], [HTMLInputElement, 'selectionStart'], [CSSStyleDeclaration, 'parentRule']];
  check('each is a getter on its prototype', pairs.map(([C, n]) => typeof Object.getOwnPropertyDescriptor(C.prototype, n).get), pairs.map(() => 'function'));
  const descriptor = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'paused');
  check('a wrapper can add a setter to one', Object.keys(Object.defineProperty({}, 'x', { ...descriptor, set() {} })), ['x']);
  const one = document.createElement('video');
  const two = document.createElement('video');
  one.currentTime = 5;
  check('a settable one keeps what each element was given', [one.currentTime, two.currentTime, one.paused, one.videoWidth], [5, 0, true, 0]);
</script>"#);
    assert_all_ok(&lines, 3);
}

#[test]
fn the_platforms_own_methods_do_not_call_what_a_page_patched() {
    let lines = run(r#"<div id="host"><p id="first"></p></div><script>
  const calls = [];
  for (const name of ['appendChild', 'insertBefore', 'replaceChild', 'removeChild']) {
    const original = Node.prototype[name];
    Node.prototype[name] = function (...args) { calls.push(name); return original.apply(this, args); };
  }
  const host = document.getElementById('host');
  const first = document.getElementById('first');
  first.after(document.createElement('b'));
  first.before('text');
  host.append(document.createElement('i'));
  host.prepend(document.createElement('u'));
  first.replaceWith(document.createElement('s'));
  host.lastChild.insertAdjacentElement('afterend', document.createElement('em'));
  host.replaceChildren(document.createElement('hr'));
  check('none of them went through the patched methods', calls, []);
  host.appendChild(document.createElement('br'));
  check('the page still hears its own calls', calls, ['appendChild']);
  check('the tree changed as asked', host.innerHTML, '<hr><br>');
</script>"#);
    assert_all_ok(&lines, 3);
}

#[test]
fn nodes_look_up_their_namespaces() {
    let lines = run(
        r#"<div id="host"><svg xmlns:xl="http://www.w3.org/1999/xlink"><g></g></svg></div><script>
  const host = document.getElementById('host');
  const g = host.querySelector('g');
  check('an HTML element is in the HTML namespace', [document.body.lookupNamespaceURI(null), document.lookupNamespaceURI(''), host.firstChild.lookupNamespaceURI(undefined)], ['http://www.w3.org/1999/xhtml', 'http://www.w3.org/1999/xhtml', 'http://www.w3.org/2000/svg']);
  check('a declared prefix is found from below it', [g.lookupNamespaceURI('xl'), g.lookupPrefix('http://www.w3.org/1999/xlink'), host.lookupNamespaceURI('xl'), g.lookupNamespaceURI('xml')], ['http://www.w3.org/1999/xlink', 'xl', null, 'http://www.w3.org/XML/1998/namespace']);
  check('the default namespace is told apart', [document.body.isDefaultNamespace('http://www.w3.org/1999/xhtml'), g.isDefaultNamespace('http://www.w3.org/1999/xhtml'), document.createDocumentFragment().lookupNamespaceURI(null)], [true, false, null]);
</script>"#,
    );
    assert_all_ok(&lines, 3);
}

#[test]
fn the_windows_own_attributes_are_accessors() {
    let lines = run(r#"<script>
  const names = ['window', 'self', 'top', 'parent', 'frames', 'opener', 'frameElement', 'closed', 'name'];
  check('each has a getter on the window', names.map((n) => typeof Object.getOwnPropertyDescriptor(window, n).get), names.map(() => 'function'));
  const closed = Object.getOwnPropertyDescriptor(window, 'closed').get;
  check('they answer as a window in no frame', [window === self, top === window, parent === window, closed.call(window), frameElement, opener], [true, true, true, false, null, null]);
  name = 'tab';
  self = 1;
  check('name is kept and self is replaceable', [window.name, self, window.self], ['tab', 1, 1]);
</script>"#);
    assert_all_ok(&lines, 3);
}

#[test]
fn documents_script_makes_are_documents_of_their_own() {
    let lines = run(r#"<title>Page</title><script>
  const made = document.implementation.createHTMLDocument('Made');
  check('a document', [made instanceof Document, made instanceof HTMLDocument, made.nodeType, made.nodeName], [true, true, 9, '#document']);
  check('not the page', [made === document, made.ownerDocument, made.defaultView, made.URL, made.location], [false, null, null, 'about:blank', null]);
  check('its own shape', [made.doctype.name, made.documentElement.localName, made.head.localName, made.body.localName, made.title], ['html', 'html', 'head', 'body', 'Made']);
  check('the page keeps its title', document.title, 'Page');
  check('its nodes are its own', [made.body.ownerDocument === made, made.createElement('p').ownerDocument === made, made.createTextNode('t').ownerDocument === made], [true, true, true]);
  check('in it is connected to it', made.body.isConnected, true);
  check('its own implementation', [made.implementation !== document.implementation, made.implementation.createDocumentType('x', '', '').ownerDocument === made], [true, true]);
  made.cookie = 'stolen=1';
  check('no cookies of its own, and none written to the page', [made.cookie, document.cookie.includes('stolen')], ['', false]);
  check('complete as soon as made', made.readyState, 'complete');

  const moved = made.createElement('b');
  made.body.appendChild(moved);
  check('found in it', made.querySelector('b') === moved && made.getElementsByTagName('b').length === 1, true);
  document.body.appendChild(moved);
  check('moved into the page, it is the page\'s', moved.ownerDocument === document, true);
  moved.remove();
  check('and stays the page\'s when taken out', moved.ownerDocument === document, true);
  const kept = made.body.appendChild(made.createElement('i'));
  kept.remove();
  check('taken out of it, it stays its', kept.ownerDocument === made, true);
  check('importNode copies for the page', document.importNode(kept, true).ownerDocument === document, true);
  check('adoptNode takes it', document.adoptNode(kept).ownerDocument === document, true);

  const script = made.createElement('script');
  script.textContent = 'console.log("ran in a made document")';
  made.body.appendChild(script);

  const parsed = new DOMParser().parseFromString('<!doctype html><title>P</title><p id=a>one<script>console.log("ran from DOMParser")<\/script>', 'text/html');
  check('parsed html', [parsed.doctype.name, parsed.title, parsed.getElementById('a').firstChild.data, parsed.body.children.length], ['html', 'P', 'one', 1]);
  check('parsed into a document of its own', [parsed !== document, parsed.contentType, parsed.querySelector('p').ownerDocument === parsed], [true, 'text/html', true]);
  document.body.appendChild(document.importNode(parsed.getElementById('a'), true));

  const xml = new DOMParser().parseFromString('<?xml version="1.0"?><Root xmlns:a="urn:a"><Item n="1"/><a:Thing a:k="v">t</a:Thing></Root>', 'application/xml');
  check('an XML document', [xml instanceof XMLDocument, xml.contentType, xml.documentElement.localName, xml.documentElement.namespaceURI], [true, 'application/xml', 'Root', null]);
  const thing = xml.documentElement.lastChild;
  check('names as written, namespaces as declared', [thing.prefix, thing.localName, thing.namespaceURI, thing.getAttributeNS('urn:a', 'k')], ['a', 'Thing', 'urn:a', 'v']);
  check('case kept', [xml.getElementsByTagName('Item').length, xml.getElementsByTagName('item').length], [1, 0]);
  const broken = new DOMParser().parseFromString('<a><b></a>', 'text/xml');
  check('not well formed', broken.getElementsByTagName('parsererror').length, 1);
  try { new DOMParser().parseFromString('', 'text/plain'); console.log('FAIL a bad type: did not throw'); }
  catch (error) { check('a bad type', error instanceof TypeError, true); }

  const created = document.implementation.createDocument(null, 'root', null);
  const element = created.createElement('Mixed');
  check('createDocument', [created instanceof XMLDocument, created.documentElement.localName, element.localName, element.namespaceURI], [true, 'root', 'Mixed', null]);

  const written = document.implementation.createHTMLDocument('');
  written.open();
  written.write('<p id=w>hi');
  check('readable before close', written.getElementById('w').textContent, 'hi');
  written.write(' there</p>');
  written.close();
  check('written in parts', written.getElementById('w').textContent, 'hi there');
  check('the page is not written to', [document.write('<i>no</i>'), document.title], [undefined, 'Page']);
</script>"#);
    assert_all_ok(&lines, 26);
}
