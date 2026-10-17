// ---------------------------------------------------------------- Sealing
// Last: what the other files left on `Node.prototype` is what the glue's own
// methods use from here on (see `treeOps` in `prelude.js`).

for (const name of ['appendChild', 'insertBefore', 'replaceChild', 'removeChild']) {
  treeOps[name] = Node.prototype[name];
}
Object.freeze(treeOps);
