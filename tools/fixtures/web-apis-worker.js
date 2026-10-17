// The worker tools/fixtures/web-apis.html starts: it imports a script, fetches
// a file through the page, and answers each message with what it found.
importScripts('web-apis-worker-import.js');
onmessage = async (event) => {
  const text = await (await fetch('web-apis-worker-import.js')).text();
  const echo = event.data;
  postMessage([echo.self === echo, echo.when instanceof Date, Array.from(echo.bytes), imported, text.includes('imported'), typeof document].join(','));
};
