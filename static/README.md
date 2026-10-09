# Static assets

Copied from [nyaadevs/nyaa](https://github.com/nyaadevs/nyaa) at commit
`4fe0ff5b1aa7ec7c9bb2667d97e10ce2a318c676` (`nyaa/static/`), so pages look and behave like upstream.

- `css/bootstrap.min.css`, `css/bootstrap-dark.min.css`, `fonts/glyphicons-*`: Bootstrap 3 (MIT),
  customized upstream.
- `js/bootstrap-select.js`: bootstrap-select (MIT), with upstream's border-radius tweak.
- `css/main.css`, `css/bootstrap-xl-mod.css`, `js/main.js`, `img/`, `favicon.png`, `pinned-tab.svg`:
  nyaadevs/nyaa (GPL-3.0). `js/main.js` has one local change: the info bubble code returns early
  when no `#infobubble` element is on the page.

jQuery, Bootstrap's JS, markdown-it, Font Awesome and bootstrap-select's CSS load from cdnjs,
as upstream does (see `templates/layout.html`).

`altcha/`: the [ALTCHA](https://altcha.org) captcha widget 3.2.3 (MIT, `LICENSE.txt`), from the
npm package's `dist/external` build (`altcha.min.js`, `altcha.css`) and `dist/workers/sha.js`, so
it needs no inline workers or third-party hosts. `setup.js` is ours: it loads the widget and
registers the worker.
