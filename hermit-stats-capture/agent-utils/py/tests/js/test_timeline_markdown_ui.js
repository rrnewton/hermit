"use strict";

const assert = require("assert");
const fs = require("fs");
const path = require("path");

const repoPy = path.resolve(__dirname, "..", "..");
const staticRoot = path.join(repoPy, "wrkviz", "static");
const bundlePath = path.join(
  staticRoot,
  "vendor",
  "markdown-it-15.0.0.min.js"
);
const markdownit = require(bundlePath);
const renderer = markdownit({
  html: false,
  linkify: true,
  typographer: false,
  breaks: false
});

const rendered = renderer.render(
  "# Release summary\n\n" +
  "> Eight focused cases passed.\n\n" +
  "| Area | Result |\n| --- | --- |\n| Parser | **Pass** |\n\n" +
  "```python\nprint('verified')\n```\n\n" +
  "<script>alert('unsafe')</script>\n\n" +
  "[unsafe](javascript:alert(1))\n"
);

assert.match(rendered, /<h1>Release summary<\/h1>/);
assert.match(rendered, /<blockquote>/);
assert.match(rendered, /<table>/);
assert.match(rendered, /<strong>Pass<\/strong>/);
assert.match(rendered, /class="language-python"/);
assert.ok(!rendered.includes("<script>"), "raw HTML must remain disabled");
assert.match(rendered, /&lt;script&gt;/);
assert.ok(!rendered.includes("href=\"javascript:"), "dangerous links must be rejected");

const index = fs.readFileSync(path.join(staticRoot, "index.html"), "utf8");
assert.ok(
  index.indexOf("vendor/markdown-it-15.0.0.min.js") < index.indexOf("app.js"),
  "the pinned renderer must load before the application"
);
assert.match(index, /<link rel="icon" href="data:,">/);
const app = fs.readFileSync(path.join(staticRoot, "app.js"), "utf8");
assert.match(app, /markdownit\(\{[\s\S]*html: false/);
assert.match(app, /linkify: false/);
assert.match(app, /renderer\.rules\.link_open/);
assert.match(app, /renderer\.rules\.image/);
assert.match(app, /removeUnexpectedMarkdownLinks\(container\)/);
assert.ok(
  app.indexOf("removeUnexpectedMarkdownLinks(container)") <
    app.indexOf("linkKnownGlossaryTerms(container)"),
  "untrusted Markdown links must be removed before known glossary links are created"
);
assert.match(app, /markdownElement\(loaded\.content, "markdown-document", false\)/);
assert.match(app, /label: "Markdown Summary"/);
assert.match(app, /label: "Technical"/);
assert.match(app, /label: "Plain Language"/);
assert.match(app, /linkKnownGlossaryTerms\(container\)/);
assert.ok(
  (app.match(/linkKnownGlossaryTerms\(container\)/g) || []).length >= 2,
  "glossary links must cover rendered Markdown and structured summary text"
);
assert.match(app, /#glossary\/" \+ match\.id/);
assert.match(app, /glossary entry has an invalid stable target/);
assert.match(index, /id="glossary-open"/);
assert.match(index, /id="site-title"/);
assert.match(index, /id="site-identity-details"/);
assert.match(app, /Agent Timeline: /);
assert.doesNotMatch(
  app,
  /browser local/,
  "timeline labels must use the archive timezone rather than ambient browser time"
);
assert.ok(
  app.indexOf("window.location.href,\n      preferredBase") >= 0,
  "archive-root paths must be tried before detail-relative fallbacks"
);

console.log("timeline Markdown rendering tests passed");
