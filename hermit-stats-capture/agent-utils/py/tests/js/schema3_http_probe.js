"use strict";

// Drive `static/app.js`'s schema-3 reader against a real archive over real HTTP.
//
// Invoked as `node schema3_http_probe.js <base-url> <request.json>` by
// `tests/test_timeline_v3_website.py`, which writes the shards with this repository's own
// `timeline_v3.write_timeline_v3` and serves them with the archive's own `serve.py`. Nothing here
// reimplements either side: the bytes on disk were written by the writer under test, the 206s and
// `If-Range` handling come from the server under test, and every function that reads them is
// sliced out of the bundle under test. What this file contributes is the wiring and an account of
// what was transferred, printed as JSON on stdout for the Python assertions to read.
//
// It prints exactly one line of JSON. Anything else on stdout would be a bug in this file, so the
// caller is entitled to `json.loads` the whole of it.

const assert = require("assert");
const fs = require("fs");
const http = require("http");

const { loadReader } = require("./schema3_probe.js");

function fetchWithHttp(url, init, requestFunction) {
  return new Promise(function (resolve, reject) {
    let response = null;
    let settled = false;
    const makeRequest = requestFunction || http.request;
    const outgoing = makeRequest(url, {
      method: "GET",
      headers: (init && init.headers) || {}
    });

    function cleanup() {
      outgoing.removeListener("error", fail);
      if (response !== null) {
        response.removeListener("data", collect);
        response.removeListener("aborted", incomplete);
        response.removeListener("error", responseFailed);
        response.removeListener("end", finish);
        response.removeListener("close", closed);
      }
    }

    function fail(error) {
      if (settled) {
        return;
      }
      settled = true;
      cleanup();
      if (response !== null && !response.destroyed) {
        response.destroy();
      }
      if (!outgoing.destroyed) {
        outgoing.destroy();
      }
      reject(error instanceof Error ? error : new Error(String(error)));
    }

    function incomplete() {
      fail(new Error("HTTP response ended before its declared body was complete: " + url));
    }

    function responseFailed(error) {
      fail(new Error(
        "HTTP response failed before its declared body was complete: " +
          String((error && error.message) || error)
      ));
    }

    function closed() {
      if (response !== null && !response.complete) {
        incomplete();
      }
    }

    const chunks = [];
    let received = 0;
    function collect(chunk) {
      const data = Buffer.from(chunk);
      chunks.push(data);
      received += data.byteLength;
    }

    function finish() {
      assert.notStrictEqual(response, null);
      const declaredRaw = response.headers["content-length"];
      const declared = Array.isArray(declaredRaw) ? declaredRaw[0] : declaredRaw;
      if (declared !== undefined && Number(declared) !== received) {
        incomplete();
        return;
      }
      settled = true;
      cleanup();
      const buffer = Buffer.concat(chunks);
      resolve({
        ok: response.statusCode >= 200 && response.statusCode < 300,
        status: response.statusCode,
        statusText: response.statusMessage || "",
        headers: {
          get: function (name) {
            const value = response.headers[String(name).toLowerCase()];
            if (Array.isArray(value)) {
              return value.join(", ");
            }
            return value === undefined ? null : String(value);
          }
        },
        arrayBuffer: async function () {
          return buffer.buffer.slice(
            buffer.byteOffset,
            buffer.byteOffset + buffer.byteLength
          );
        },
        text: async function () { return buffer.toString("utf8"); },
        json: async function () { return JSON.parse(buffer.toString("utf8")); }
      });
    }

    outgoing.once("error", fail);
    outgoing.once("response", function (incoming) {
      response = incoming;
      incoming.on("data", collect);
      incoming.once("aborted", incomplete);
      incoming.once("error", responseFailed);
      incoming.once("end", finish);
      incoming.once("close", closed);
    });
    outgoing.end();
  });
}

function fetchForNode(candidate) {
  return typeof candidate === "function" ? candidate.bind(global) : fetchWithHttp;
}

//: Count what crosses the wire, and against which path, so the Python side can assert on the
//: shape of the traffic and not merely on the answer.
const traffic = [];

function mark() {
  return traffic.length;
}

//: The bytes transferred since *start* against paths containing *needle*, which is how the cost
//: of one stream is separated from the cost of a search as a whole. `search-bloom/` is the only
//: caller today and it is the point of the prefilter's move out of the bootstrap: a query that
//: cannot form a trigram must transfer zero of it.
function bytesMatching(start, needle) {
  return traffic.slice(start).reduce(function (total, item) {
    return item.path.indexOf(needle) >= 0 ? total + item.bytes : total;
  }, 0);
}

function since(start) {
  const slice = traffic.slice(start);
  return {
    requests: slice.length,
    bytes: slice.reduce(function (total, item) { return total + item.bytes; }, 0),
    ranged: slice.filter(function (item) { return item.ranged; }).length,
    conditional: slice.filter(function (item) { return item.conditional; }).length,
    paths: slice.map(function (item) { return item.path; }),
    statuses: slice.map(function (item) { return item.status; })
  };
}

async function main() {
  const baseUrl = process.argv[2];
  const request = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
  assert.ok(baseUrl, "usage: schema3_http_probe.js <base-url> <request.json>");
  const nativeFetch = fetchForNode(global.fetch);
  global.fetch = async function (url, init) {
    const absolute = new URL(url, baseUrl).href;
    const response = await nativeFetch(absolute, init);
    const buffer = await response.arrayBuffer();
    traffic.push({
      path: String(url),
      status: response.status,
      bytes: buffer.byteLength,
      ranged: Boolean(init && init.headers && init.headers.Range),
      conditional: Boolean(init && init.headers && init.headers["If-Range"])
    });
    return {
      ok: response.ok,
      status: response.status,
      statusText: response.statusText,
      headers: response.headers,
      arrayBuffer: async function () { return buffer; },
      text: async function () { return Buffer.from(buffer).toString("utf8"); },
      json: async function () { return JSON.parse(Buffer.from(buffer).toString("utf8")); }
    };
  };
  const reader = loadReader();
  const app = reader.app;

  const bootstrapStart = mark();
  const bootstrapResponse = await global.fetch(reader.SCHEMA_3_URL, {});
  const bootstrap = await bootstrapResponse.json();
  const bootstrapCost = since(bootstrapStart);

  const spineShards = reader.schema3Stream(bootstrap, "spine", true);
  const timelineShards = reader.schema3Stream(bootstrap, "timeline", true);
  const binsShards = reader.schema3Stream(bootstrap, "bins", true);
  const searchShards = reader.schema3Stream(bootstrap, "search", false);
  const bloomShards = reader.schema3Stream(bootstrap, "search_bloom", false);
  const linkShards = reader.schema3Stream(bootstrap, "search_links", false);

  app.spineByTeam = new Map(spineShards.map(function (e) { return [e.team, e]; }));
  app.searchBloomByTeam = new Map(bloomShards.map(function (e) { return [e.team, e]; }));
  app.searchLinksByTeam = new Map(linkShards.map(function (e) { return [e.team, e]; }));

  // --- the first paint -------------------------------------------------------------------
  const paintStart = mark();
  const sink = {
    agents: [], phaseCards: [], edges: [], rollups: [],
    projects: [], summary_files: [], glossary: [], project_overviews: []
  };
  const loaded = await Promise.all([
    Promise.all(spineShards.map(function (entry) {
      return Promise.all(
        reader.spineFirstPaintRanges(entry).map(function (range) {
          return reader.readShardRecords(entry, range);
        })
      ).then(function (pages) {
        return {
          team: entry.team,
          records: pages.reduce(function (all, page) { return all.concat(page); }, [])
        };
      });
    })),
    binsShards.length
      ? reader.readShardRecords(binsShards[0], { start_ms: NaN, end_ms: NaN })
      : Promise.resolve([])
  ]);
  loaded[0].forEach(function (shard) {
    reader.schema3InstallSpine(shard.team, shard.records, sink);
  });
  const paintCost = since(paintStart);

  // --- one detail window ------------------------------------------------------------------
  const window = request.window;
  const detailStart = mark();
  const selected = timelineShards.filter(function (entry) {
    return entry.start_ms < window.end_ms && entry.end_ms > window.start_ms;
  });
  const detail = [];
  for (const entry of selected) {
    const records = await reader.readShardRecords(entry, {
      start_ms: window.start_ms,
      end_ms: window.end_ms
    });
    records.forEach(function (record) { detail.push(record); });
  }
  const detailCost = since(detailStart);

  // --- the phase cards, which the first paint deliberately did not read ---------------------
  const cardStart = mark();
  const cards = [];
  for (const entry of spineShards) {
    const records = await reader.readShardKind(entry, "phase_card");
    records.forEach(function (record) { cards.push(record.id); });
  }
  const cardCost = since(cardStart);

  // --- the zoom bounds, which the first paint deliberately did not read ---------------------
  const boundsStart = mark();
  await Promise.all(spineShards.map(function (entry) {
    return reader.readShardKind(entry, "activity_bounds").then(function (records) {
      records.forEach(function (record) {
        app.activityBoundsByRef.set(record.ref, {
          activity_start_ms: record.activity_start_ms,
          activity_end_ms: record.activity_end_ms
        });
      });
    });
  }));
  const boundsCost = since(boundsStart);

  // --- the transcript search corpus, through the page's own search ------------------------
  //
  // Every function here is the bundle's. An earlier form of this block substituted
  // `record.text.toLowerCase().indexOf(query.toLowerCase())` for `updateTranscriptSearch` and
  // reported every prompt in the links shard as this query's excerpts, which meant the browser --
  // the second reader of the corpus, and the stated reason the schema-2 writer could be retired
  // -- was asserted against a matcher and a linkage the page does not have. A regression inside
  // `ensureSearchLinks` (one half of the shard dropped, the two line ranges swapped) would have
  // left every assertion passing. So this drives `loadSchema3` and then
  // `requestTranscriptSearchCorpus`, the two entry points the page itself uses, and reports what
  // the result list would have shown.
  const searchReport = { published: searchShards.length > 0, searches: [] };
  for (const spec of (request.searches || [])) {
    const local = loadReader({
      query: spec.query,
      selectedTeam: spec.team || "",
      searchScope: spec.scope || "all-transcript",
      searchSort: spec.sort || "relevance"
    });
    const loadStart = mark();
    const accepted = await local.loadSchema3();
    const queryStart = mark();
    await local.requestTranscriptSearchCorpus();
    const app2 = local.app;
    //: What one result row renders from: the record's own ref, the replies the linkage says the
    //: prompt it belongs to received, and the prompt excerpt shown above a matched response.
    //: These are the three fields a broken relationship sidecar gets wrong *without erroring*.
    const rows = app2.transcriptSearchResults.map(function (item) {
      const record = item.record;
      const promptRef = String(record.record_type) === "prompt"
        ? String(record.ref)
        : String(record.prompt_ref || "");
      const responses = app2.searchResponsesByPrompt.get(String(record.ref)) || [];
      const excerpt = promptRef && promptRef !== String(record.ref)
        ? app2.searchPromptExcerpts.get(promptRef)
        : undefined;
      return {
        ref: String(record.ref),
        record_type: String(record.record_type),
        team: String(record.team || ""),
        linked_response_count: responses.length,
        prompt_ref: promptRef,
        has_prompt_excerpt: typeof excerpt === "string",
        prompt_excerpt: typeof excerpt === "string" ? excerpt : null
      };
    });
    rows.sort(function (left, right) { return left.ref.localeCompare(right.ref); });
    searchReport.searches.push({
      query: spec.query,
      team: spec.team || "",
      scope: spec.scope || "all-transcript",
      accepted: accepted,
      corpus_mode: app2.searchCorpusMode,
      corpus_note: app2.searchCorpusNote,
      state: app2.transcriptSearchState,
      error: app2.transcriptSearchError,
      uses_bloom: local.queryCanUseBloom(spec.query),
      bloom_bytes: bytesMatching(queryStart, "/search-bloom/"),
      shards_total: app2.searchCatalog.length,
      shards_selected: local.transcriptSearchShards().length,
      shards_loaded: app2.loadedSearchShardUrls.size,
      link_shards_loaded: app2.loadedSearchLinkUrls.size,
      needs_load_after: local.transcriptSearchNeedsLoad(),
      total: app2.transcriptSearchTotal,
      rows: rows,
      load_cost: since(loadStart),
      cost: since(queryStart)
    });
  }

  // --- and finally the whole loader, end to end ---------------------------------------------
  //
  // Everything above drives the reader's parts. This drives `loadSchema3` itself -- the function
  // that turns a bootstrap into the object the page renders -- because an assembly is where a
  // misspelled name survives both the unit tests and `node --check`. The DOM boundary is the one
  // thing stubbed: `initializeData` is captured rather than executed.
  const loader = loadReader();
  const loaderStart = mark();
  const accepted = await loader.loadSchema3();
  const loaderCost = since(loaderStart);
  const merged = loader.installed[0] || null;

  process.stdout.write(JSON.stringify({
    bootstrap: {
      cost: bootstrapCost,
      schema_version: bootstrap.schema_version,
      kind: bootstrap.kind,
      teams: (bootstrap.teams || []).map(function (team) { return team.slug; })
    },
    streams: {
      timeline: timelineShards.length,
      spine: spineShards.length,
      bins: binsShards.length,
      search: searchShards.length,
      search_bloom: bloomShards.length,
      search_links: linkShards.length
    },
    first_paint: {
      cost: paintCost,
      prefix: spineShards.map(function (entry) {
        const ranges = reader.spineFirstPaintRanges(entry);
        return {
          team: entry.team,
          ranges: ranges,
          lines: ranges.reduce(function (total, range) { return total + range.count; }, 0),
          records: entry.records,
          bounds: entry.line_ranges.has("activity_bounds")
            ? entry.line_ranges.get("activity_bounds").count
            : 0,
          cards: entry.line_ranges.has("phase_card")
            ? entry.line_ranges.get("phase_card").count
            : 0
        };
      }),
      agents: sink.agents.map(function (agent) { return agent.id; }).sort(),
      phase_cards: sink.phaseCards.map(function (card) { return card.id; }).sort(),
      phase_card_has_states: sink.phaseCards.some(function (card) {
        return Object.prototype.hasOwnProperty.call(card, "states");
      }),
      rollups: sink.rollups.length,
      glossary: sink.glossary.length,
      summary_files: sink.summary_files.length,
      edges: sink.edges.map(function (edge) { return edge.id; }).sort(),
      bins: loaded[1].length,
      envelope_leaked: loaded[1].concat(sink.agents).some(function (record) {
        return Object.prototype.hasOwnProperty.call(record, "record_kind");
      })
    },
    detail: {
      cost: detailCost,
      shards_selected: selected.length,
      shards_total: timelineShards.length,
      records: detail.map(function (record) {
        return { kind: record.record_kind, id: record.id, at_ms: record.at_ms };
      }).sort(function (left, right) {
        return left.at_ms - right.at_ms || String(left.id).localeCompare(String(right.id));
      })
    },
    phase_cards: {
      cost: cardCost,
      ids: cards.sort()
    },
    activity_bounds: {
      cost: boundsCost,
      count: app.activityBoundsByRef.size,
      sample: Array.from(app.activityBoundsByRef.entries()).sort().slice(0, 4)
    },
    search: searchReport,
    loader: {
      accepted: accepted,
      cost: loaderCost,
      schema_mode: loader.app.schemaMode,
      installed: merged === null ? null : {
        schema_version: merged.schema_version,
        source_digest: merged.source_digest,
        display_timezone: merged.display_timezone,
        range: merged.range,
        teams: (merged.teams || []).map(function (team) { return team.slug; }),
        agents: merged.agents.length,
        rollups: merged.rollups.length,
        glossary: merged.glossary.length,
        summary_files: merged.summary_files.length,
        edges: merged.edges.length,
        activity_bins: merged.activity_bins.length,
        phases: merged.phases.length,
        events: merged.events.length,
        stats: merged.stats
      },
      shard_catalog: loader.app.shardCatalog.length,
      search_catalog: loader.app.searchCatalog.length,
      phase_index_ready: loader.app.phaseIndexReady,
      spine_teams: Array.from(loader.app.spineByTeam.keys()).sort()
    }
  }) + "\n");
}

if (require.main === module) {
  main().catch(function (error) {
    process.stderr.write(String((error && error.stack) || error) + "\n");
    process.exitCode = 1;
  });
}

module.exports = { fetchForNode: fetchForNode, fetchWithHttp: fetchWithHttp };
