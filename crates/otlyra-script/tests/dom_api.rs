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
