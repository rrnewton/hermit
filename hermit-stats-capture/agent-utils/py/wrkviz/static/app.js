(function () {
  "use strict";

  var DATA_URL = "data/timeline.json";
  var SCHEMA_2_URL = "data/timeline-v2.json";
  var SCHEMA_3_URL = "data/timeline-v3.json";
  // The sidecar's own header constants, restated here rather than discovered, so a shard written
  // by a future codec is *declined* instead of misread. `seekable_jsonl` publishes both in every
  // index header and the bootstrap's `codec` block repeats the container; a reader that accepted
  // whatever it found would inflate zstd members as gzip and report the truncation as damage.
  var SCHEMA_3_INDEX_FORMAT = "wrkviz/seekable-jsonl-index";
  var SCHEMA_3_INDEX_VERSION = 1;
  var SCHEMA_3_INDEX_SUFFIX = ".index.jsonl";
  var SCHEMA_3_ROOT = "data/timeline-v3/";
  // How many member fetches are in flight at once, **across the page** and not per shard read.
  // The same bound, and the same reason, as SEARCH_LOAD_CONCURRENCY: a first paint asks for one
  // member from each of a dozen spine shards and a browser that opened all of them at once would
  // queue them behind each other anyway.
  //
  // It has to be global to be that bound at all. `readShardRecords` bounds the members of *one*
  // shard, and a first paint calls it once per spine shard inside a single `Promise.all` -- so on
  // the twelve-team archive the per-call bound alone permits 78 concurrent fetches, and
  // `requestSearchCorpus` maps over the whole 72-entry shard catalogue with no bound above it at
  // all. The per-origin cap the browser imposes hid that, but a constant whose comment states an
  // invariant the code does not have is what a later change gets sized against, so the invariant
  // is enforced instead: `schema3MemberSlot` below is the one gate every member fetch passes.
  var SCHEMA_3_MEMBER_CONCURRENCY = 6;
  var SCHEMA_3_DAY_MS = 24 * 60 * 60 * 1000;
  // The one `schema3Schema2SearchFallback` outcome that is not worth a line in the meta: an
  // archive exported with no transcript search corpus at all has nothing surprising to report,
  // and every other outcome does. Named rather than compared as a literal in two places.
  var SCHEMA_2_CORPUS_ABSENT = "no schema-2 corpus beside it";
  var SVG_NS = "http://www.w3.org/2000/svg";
  var ROW_HEIGHT = 54;
  var PHASE_TOP = 7;
  var PHASE_HEIGHT = 38;
  var COMPACT_PHASE_TOP = 16;
  var COMPACT_PHASE_HEIGHT = 31;
  var COMPACT_LABEL_WIDTH = 72;
  var AGGREGATE_LABEL_WIDTH = 128;
  var AGGREGATE_TEAM_HEIGHT = 78;
  // A team block is as tall as the team was big: **linear** in agents present, at this many
  // pixels each. Three concurrent agents draw three times the height of one, which is the only
  // rule under which a reader can compare two blocks by eye and get the ratio right. The height
  // this used to have was `log2(1 + average)`, and under a log there is no ratio to read: three
  // agents drew twice one agent, ten drew three and a half times it, and the picture quietly
  // said "busier" while refusing to say how much.
  //
  // SIX pixels, chosen against the concurrency this tool actually sees rather than picked for
  // roundness. On a 2,656-agent archive the per-hour average present concurrency has a median of
  // 1.9, a p90 of 6.0 and a p99 of 9.5 agents. Six pixels makes the median block 11px -- a bar,
  // not a hairline -- puts the p90 at 36px, and reaches the cap just past the p99. Ten pixels
  // would saturate at five agents and flatten the whole busy half of the distribution together;
  // three would draw the median as a 6px sliver.
  var AGGREGATE_PIXELS_PER_AGENT = 6;
  // What a log scale bought, and what has to be paid for now that it is gone: the compression of
  // outliers. Linear height and a 78px row cannot both be honoured by a 60-agent burst, so the
  // block is clamped here -- at nine agents, the row's honest capacity -- and a clamped block is
  // drawn with a torn top edge (`activity-bin-overflow`) so the reader is TOLD the bar continues
  // past what is shown instead of being handed a bar that quietly ties with a nine-agent one.
  // On the archive above that is ~1.5% of bins: rare enough to read as an exception, common
  // enough that a reader will meet the convention and learn it.
  var AGGREGATE_BIN_MAX_HEIGHT = 54;
  // A floor, not a measurement. A bin with a trace of activity still has to be a click and hover
  // target, so it is drawn at 4px even though its agent count says less. Blocks at the floor are
  // NOT proportional to each other and are not meant to be read against each other.
  var AGGREGATE_BIN_MIN_HEIGHT = 4;
  var STATE_HEIGHT = 6;
  var MIN_VIEW_MS = 1000;
  var SEARCH_RESULT_LIMIT = 100;
  var SEARCH_JUMP_SPAN_MS = 30 * 60 * 1000;
  var SEARCH_EXCERPT_CHARACTERS = 480;
  var SEARCH_INPUT_DEBOUNCE_MS = 250;
  var SEARCH_LOAD_CONCURRENCY = 6;
  var TRIGRAM_BLOOM_ALGORITHM = "ascii-lower-utf8-trigram-fnv1a32-double-v1";
  var TRIGRAM_BLOOM_HASH_COUNT = 7;
  var FNV_OFFSET = 2166136261;
  var FNV_PRIME = 16777619;
  var SECOND_HASH_SEED = 0x9e3779b9;
  var PHASE_COLORS = [
    "#287ca3",
    "#555ec4",
    "#7c4db5",
    "#267d72",
    "#a06931",
    "#386da8",
    "#8d4d79",
    "#39794d"
  ];
  var STATE_COLORS = {
    active: "#3bc983",
    tool: "#e7ad43",
    waiting: "#5d8ff4",
    idle: "#68758b",
    blocked: "#ef6470"
  };
  var EDGE_COLORS = {
    spawn: "#4bc6e8",
    continuation: "#f6c453",
    message: "#a77bf3",
    result: "#3bc983",
    other: "#5d8ff4"
  };

  function byId(id) {
    var element = document.getElementById(id);
    if (!element) {
      throw new Error("Missing required element #" + id);
    }
    return element;
  }

  var dom = {
    card: document.querySelector(".timeline-card"),
    siteTitle: byId("site-title"),
    identityDetails: byId("site-identity-details"),
    identityList: byId("site-identity-list"),
    meta: byId("dataset-meta"),
    teamFilter: byId("team-filter"),
    search: byId("search"),
    searchScope: byId("search-scope"),
    searchResults: byId("search-results"),
    searchResultsTitle: byId("search-results-title"),
    searchResultsCount: byId("search-results-count"),
    searchResultsStatus: byId("search-results-status"),
    searchResultsList: byId("search-results-list"),
    searchSort: byId("search-sort"),
    searchResultsClose: byId("search-results-close"),
    fit: byId("fit"),
    glossaryOpen: byId("glossary-open"),
    summaryMenu: byId("summary-menu"),
    summaryFiles: byId("summary-files"),
    perAgentTracks: byId("per-agent-tracks"),
    showGlobalMessages: byId("show-global-messages"),
    showHighlightedMessages: byId("show-highlighted-messages"),
    zoomOut: byId("zoom-out"),
    zoomIn: byId("zoom-in"),
    viewRange: byId("view-range"),
    rollupRow: byId("rollup-row"),
    rollupTrack: byId("rollup-track"),
    axis: byId("time-axis"),
    scroll: byId("timeline-scroll"),
    svg: byId("timeline-svg"),
    empty: byId("empty-state"),
    statsRange: byId("stats-range-label"),
    statsValues: byId("stats-values"),
    tooltip: byId("tooltip"),
    tooltipTitle: byId("tooltip-title"),
    tooltipBody: byId("tooltip-body"),
    tooltipStats: byId("tooltip-stats"),
    modalBackdrop: byId("modal"),
    modalTitle: byId("modal-title"),
    modalEyebrow: byId("modal-eyebrow"),
    modalClose: byId("modal-close"),
    modalSummary: byId("modal-summary"),
    modalTabs: byId("modal-tabs"),
    modalContent: byId("modal-content"),
    contextMenu: byId("timeline-context-menu"),
    contextMenuTitle: byId("context-menu-title"),
    contextMenuActions: byId("context-menu-actions"),
    laneMenu: byId("lane-agent-menu"),
    laneMenuTitle: byId("lane-agent-menu-title"),
    laneMenuActions: byId("lane-agent-menu-actions"),
    loadError: byId("load-error")
  };

  var app = {
    data: null,
    viewStart: 0,
    viewEnd: 1,
    navigationRange: { start_ms: 0, end_ms: 1 },
    width: 1000,
    labelWidth: 238,
    chartWidth: 762,
    rows: [],
    laneCount: 0,
    rowByAgent: new Map(),
    phasesByAgent: new Map(),
    agentsById: new Map(),
    teamBySlug: new Map(),
    teamActivityScores: new Map(),
    summaryRangesByTeam: new Map(),
    glossaryById: new Map(),
    artifactsById: new Map(),
    artifactCatalogState: "legacy",
    artifactCatalogError: "",
    artifactCatalogPromise: null,
    axisTicks: [],
    selectedTeam: "",
    query: "",
    searchScope: "labels",
    searchSort: "relevance",
    searchCatalog: [],
    searchBloomByUrl: new Map(),
    loadedSearchShardUrls: new Set(),
    loadedSearchLinkUrls: new Set(),
    searchRecords: [],
    searchRecordsByRef: new Map(),
    searchPromptExcerpts: new Map(),
    searchResponsesByPrompt: new Map(),
    searchLinkPromptRefs: new Set(),
    searchLinkResponseRefs: new Set(),
    searchLoadQueue: [],
    searchLoadActive: 0,
    searchInputTimer: null,
    searchRequestGeneration: 0,
    transcriptSearchState: "legacy",
    transcriptSearchError: "",
    //: Which generation the *transcript search corpus* is being read out of, which is not always
    //: the generation the timeline is being read out of. Schema 3 carried no corpus until the
    //: `search`/`search_bloom`/`search_links` streams existed, so an archive built between those
    //: two moments has a schema-3 timeline beside a schema-2 corpus -- and the page reads both,
    //: for the same reason `query.TimelineQuery._search_bootstrap` does. Three states rather
    //: than a boolean, because "there is no corpus at all" is a third answer and the search
    //: dispatch has to tell it from the other two.
    searchCorpusMode: "none",
    //: Empty, or one sentence saying why a schema-3 archive's search is coming from schema 2 --
    //: or why it is coming from nowhere. Shown in the meta line, because "this archive predates
    //: the schema-3 search corpus" is a fact about the archive that only the page can see.
    searchCorpusNote: "",
    transcriptSearchResults: [],
    transcriptSearchTotal: 0,
    transcriptMatchedAgentIds: new Set(),
    activeSearchRef: "",
    perAgentTracks: false,
    showGlobalMessages: false,
    showHighlightedMessages: true,
    selection: null,
    rangeSelection: null,
    drag: null,
    suppressClickUntil: 0,
    renderQueued: false,
    renderLod: "detail",
    renderRevision: 0,
    detailRequest: 0,
    modalRestoreFocus: null,
    laneMenuAnchor: null,
    timezone: "UTC",
    timezoneStatus: "archive fallback",
    schemaMode: "schema1",
    shardCatalog: [],
    resourcePromises: new Map(),
    detailPromises: new Map(),
    loadedShardUrls: new Set(),
    phaseIds: new Set(),
    edgeIds: new Set(),
    searchShardState: "legacy",
    phaseIndexReference: null,
    phaseIndexPromise: null,
    phaseIndexByAgent: new Map(),
    phaseIndexReady: false,
    detailErrorActive: false,
    //: Schema 3 only. Sidecar indexes, keyed by their own URL, and inflated members, keyed by
    //: `<shard path>#<compressed offset>`. Both are promise caches for the same reason
    //: `resourcePromises` is one: two panes asking for the same day must produce one fetch, and
    //: a member is immutable for the lifetime of a page load because the ETag check below
    //: refuses to stitch two versions of a shard together.
    shardIndexPromises: new Map(),
    memberPromises: new Map(),
    //: The ETag of each schema-3 shard, learned from the first range response and sent back as
    //: `If-Range` on every later one. See `fetchShardMember`.
    shardEtags: new Map(),
    //: Schema 3's zoom bounds, which are a spine kind of their own rather than fields on the
    //: agent, phase card and rollup records -- so they are fetched on the first zoom, not at
    //: first paint, and land here keyed by the archive's stable reference.
    activityBoundsByRef: new Map(),
    activityBoundsPromises: new Map(),
    phaseCardPromises: new Map(),
    //: The three per-team schema-3 shards a reader addresses by team rather than by day: the
    //: spine, the search prefilter and the relationship sidecar.
    spineByTeam: new Map(),
    searchBloomByTeam: new Map(),
    searchLinksByTeam: new Map(),
    searchBloomPromises: new Map(),
    searchLinkPromises: new Map()
  };

  var timelineCore = window.AgentTimelineCore;

  var markdownRenderer = typeof window.markdownit === "function"
    ? window.markdownit({
        html: false,
        linkify: false,
        typographer: false,
        breaks: false
      })
    : null;

  if (markdownRenderer) {
    markdownRenderer.renderer.rules.link_open = function () { return ""; };
    markdownRenderer.renderer.rules.link_close = function () { return ""; };
    markdownRenderer.renderer.rules.image = function (tokens, index) {
      return markdownRenderer.utils.escapeHtml(text(tokens[index].content));
    };
  }

  function array(value) {
    return Array.isArray(value) ? value : [];
  }

  function number(value, fallback) {
    return Number.isFinite(value) ? Number(value) : fallback;
  }

  function text(value, fallback) {
    return typeof value === "string" ? value : (fallback || "");
  }

  function hasField(record, field) {
    return Boolean(record) && typeof record === "object" &&
      Object.prototype.hasOwnProperty.call(record, field);
  }

  function summaryFlagAvailable(record, field) {
    return !hasField(record, field) || record[field] !== false;
  }

  function agentSummaryAvailable(agent) {
    return summaryFlagAvailable(agent, "summary_available");
  }

  function phaseSummaryAvailable(phase) {
    return summaryFlagAvailable(phase, "summary_available");
  }

  function phaseDetailSummaryAvailable(detail, phase) {
    if (hasField(detail, "summary_available")) {
      return detail.summary_available !== false;
    }
    return phaseSummaryAvailable(phase);
  }

  function rollupSummaryAvailable(rollup) {
    if (hasField(rollup, "summary_available")) {
      return rollup.summary_available !== false;
    }
    if (hasField(rollup, "technical_summary_available") ||
        hasField(rollup, "plain_language_summary_available")) {
      return rollup.technical_summary_available !== false ||
        rollup.plain_language_summary_available !== false;
    }
    return true;
  }

  function rollupAudienceAvailable(rollup, field) {
    return rollupSummaryAvailable(rollup) && summaryFlagAvailable(rollup, field);
  }

  function recordedAgentSummaryAvailable(agent) {
    if (hasField(agent, "summary_available")) {
      return agent.summary_available !== false;
    }
    return Boolean(text(agent.lifetime_summary));
  }

  function recordedPhaseSummaryAvailable(phase) {
    if (hasField(phase, "summary_available")) {
      return phase.summary_available !== false;
    }
    return Boolean(text(phase.raw_summary_path) || text(phase.summary_path));
  }

  function recordedRollupSummaryAvailable(rollup) {
    if (hasField(rollup, "summary_available") ||
        hasField(rollup, "technical_summary_available") ||
        hasField(rollup, "plain_language_summary_available")) {
      return rollupSummaryAvailable(rollup);
    }
    return Boolean(
      text(rollup.path) ||
      text(rollup.technical_path) ||
      text(rollup.plain_language_path)
    );
  }

  function rebuildSummaryRanges() {
    var byTeam = new Map();
    function add(team, startValue, endValue) {
      var start = number(startValue, NaN);
      var end = number(endValue, NaN);
      if (!team || !Number.isFinite(start) || !Number.isFinite(end) || end <= start) {
        return;
      }
      if (!byTeam.has(team)) {
        byTeam.set(team, []);
      }
      byTeam.get(team).push({ start_ms: start, end_ms: end });
    }
    app.data.rollups.forEach(function (rollup) {
      if (recordedRollupSummaryAvailable(rollup)) {
        add(text(rollup.team), rollup.start_ms, rollup.end_ms);
      }
    });
    app.data.agents.forEach(function (agent) {
      // A coordinator lifetime can span the entire archive and would falsely imply
      // fine-grained coverage everywhere. Subagent lifetime summaries remain useful.
      if (text(agent.parent_id) && recordedAgentSummaryAvailable(agent)) {
        add(text(agent.team), agent.start_ms, agent.end_ms);
      }
    });
    app.data.phases.forEach(function (phase) {
      if (!recordedPhaseSummaryAvailable(phase)) {
        return;
      }
      var agent = app.agentsById.get(text(phase.agent_id));
      add(text(agent && agent.team), phase.start_ms, phase.end_ms);
    });
    byTeam.forEach(function (ranges) {
      ranges.sort(function (left, right) {
        return left.start_ms - right.start_ms || left.end_ms - right.end_ms;
      });
    });
    app.summaryRangesByTeam = byTeam;
  }

  function summaryAvailableInRange(team, start, end) {
    var ranges = app.summaryRangesByTeam.get(team) || [];
    for (var index = 0; index < ranges.length; index += 1) {
      var range = ranges[index];
      if (range.start_ms >= end) {
        return false;
      }
      if (range.end_ms > start) {
        return true;
      }
    }
    return false;
  }

  function rebuildTeamActivityScores() {
    var scoresByResolution = new Map();
    app.data.activity_bins.forEach(function (bin) {
      var team = text(bin.team);
      var resolution = text(bin.resolution);
      var start = number(bin.start_ms, NaN);
      var end = number(bin.end_ms, NaN);
      if (!team || !resolution || !Number.isFinite(start) ||
          !Number.isFinite(end) || end <= start) {
        return;
      }
      if (!scoresByResolution.has(team)) {
        scoresByResolution.set(team, new Map());
      }
      var scores = scoresByResolution.get(team);
      var concurrency = Math.max(0, number(
        bin.avg_present_concurrency,
        number(
          bin.avg_active_concurrency,
          number(bin.activity_evidence_fraction,
            number(bin.activity_coverage_fraction, 0))
        )
      ));
      scores.set(
        resolution,
        number(scores.get(resolution), 0) + concurrency * (end - start)
      );
    });
    var totals = new Map();
    scoresByResolution.forEach(function (scores, team) {
      var resolution = scores.has("daily")
        ? "daily"
        : (scores.has("hourly") ? "hourly" : "weekly");
      totals.set(team, number(scores.get(resolution), 0));
    });
    app.teamActivityScores = totals;
  }

  function compareTeamsByActivity(left, right) {
    var leftSlug = text(left && left.slug);
    var rightSlug = text(right && right.slug);
    return teamActivitySortScore(rightSlug) - teamActivitySortScore(leftSlug) ||
      text(left && left.label, leftSlug).localeCompare(
        text(right && right.label, rightSlug),
        undefined,
        { numeric: true }
      );
  }

  function teamActivitySortScore(slug) {
    var team = app.teamBySlug.get(slug);
    var eventCount = number(team && team.stats && team.stats.events, NaN);
    if (Number.isFinite(eventCount)) {
      return eventCount;
    }
    return number(app.teamActivityScores.get(slug), 0);
  }

  function htmlElement(tag, className, content) {
    var element = document.createElement(tag);
    if (className) {
      element.className = className;
    }
    if (content !== undefined && content !== null) {
      element.textContent = String(content);
    }
    return element;
  }

  function removeUnexpectedMarkdownLinks(container) {
    container.querySelectorAll("a").forEach(function (link) {
      var parent = link.parentNode;
      if (!parent) {
        return;
      }
      while (link.firstChild) {
        parent.insertBefore(link.firstChild, link);
      }
      parent.removeChild(link);
    });
    container.querySelectorAll("img").forEach(function (image) {
      image.replaceWith(document.createTextNode(text(image.alt, "[image omitted]")));
    });
  }

  function markdownElement(source, className, inline) {
    var container = htmlElement(
      inline ? "span" : "article",
      (className ? className + " " : "") + "rendered-markdown"
    );
    if (!markdownRenderer) {
      container.textContent = text(source);
      container.classList.add("markdown-fallback");
      return container;
    }
    container.innerHTML = inline
      ? markdownRenderer.renderInline(text(source))
      : markdownRenderer.render(text(source));
    removeUnexpectedMarkdownLinks(container);
    linkKnownGlossaryTerms(container);
    return container;
  }

  function glossaryId(value) {
    var candidate = text(value);
    return /^term-[a-z0-9-]+$/.test(candidate) ? candidate : "";
  }

  function glossaryBoundaryCharacter(value) {
    return Boolean(value) && /[A-Za-z0-9_]/.test(value);
  }

  function glossaryMatches(value) {
    var candidates = [];
    app.glossaryById.forEach(function (entry, id) {
      if (!selectedTeamAllows(entry)) {
        return;
      }
      var term = text(entry.term);
      if (!term || glossaryTermIsAmbiguous(term)) {
        return;
      }
      var from = 0;
      while (from < value.length) {
        var start = value.indexOf(term, from);
        if (start < 0) {
          break;
        }
        var end = start + term.length;
        var before = start > 0 ? value.charAt(start - 1) : "";
        var after = end < value.length ? value.charAt(end) : "";
        if (!(glossaryBoundaryCharacter(term.charAt(0)) && glossaryBoundaryCharacter(before)) &&
            !(glossaryBoundaryCharacter(term.charAt(term.length - 1)) &&
              glossaryBoundaryCharacter(after))) {
          candidates.push({ start: start, end: end, id: id, entry: entry });
        }
        from = start + Math.max(1, term.length);
      }
    });
    candidates.sort(function (left, right) {
      return left.start - right.start ||
        (right.end - right.start) - (left.end - left.start) ||
        left.id.localeCompare(right.id);
    });
    var accepted = [];
    var cursor = 0;
    candidates.forEach(function (candidate) {
      if (candidate.start >= cursor) {
        accepted.push(candidate);
        cursor = candidate.end;
      }
    });
    return accepted;
  }

  function glossaryTermIsAmbiguous(term) {
    if (app.selectedTeam) {
      return false;
    }
    var matches = 0;
    app.glossaryById.forEach(function (entry) {
      if (text(entry.term) === term) {
        matches += 1;
      }
    });
    return matches > 1;
  }

  function linkKnownGlossaryTerms(container) {
    if (!app.glossaryById.size || !document.createTreeWalker) {
      return;
    }
    var walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
    var nodes = [];
    var current;
    while ((current = walker.nextNode())) {
      var parent = current.parentElement;
      if (parent && !parent.closest("a, button, code, pre, script, style, textarea")) {
        nodes.push(current);
      }
    }
    nodes.forEach(function (node) {
      var value = node.nodeValue || "";
      var matches = glossaryMatches(value);
      if (!matches.length || !node.parentNode) {
        return;
      }
      var fragment = document.createDocumentFragment();
      var cursor = 0;
      matches.forEach(function (match) {
        fragment.appendChild(document.createTextNode(value.slice(cursor, match.start)));
        var link = htmlElement("a", "glossary-term-link", value.slice(match.start, match.end));
        link.href = "#glossary/" + match.id;
        link.title = text(match.entry.definition, text(match.entry.context, "Open project glossary entry"));
        link.dataset.glossaryId = match.id;
        link.addEventListener("click", function (event) {
          event.preventDefault();
          openGlossaryEntry(match.entry, true);
        });
        fragment.appendChild(link);
        cursor = match.end;
      });
      fragment.appendChild(document.createTextNode(value.slice(cursor)));
      node.parentNode.replaceChild(fragment, node);
    });
  }

  function safePullRequestUrl(value) {
    try {
      var url = new URL(text(value));
      if (url.protocol !== "https:" || url.hostname.toLowerCase() !== "github.com" ||
          url.username || url.password || !/^\/[^/]+\/[^/]+\/pull\/[1-9][0-9]*\/?$/.test(url.pathname)) {
        return "";
      }
      return url.href;
    } catch (_error) {
      return "";
    }
  }

  function referenceTextElement(source, references, className) {
    var content = text(source);
    var container = htmlElement("span", className || "");
    var cursor = 0;
    array(references)
      .slice()
      .sort(function (left, right) {
        return number(left.start, 0) - number(right.start, 0);
      })
      .forEach(function (reference) {
        var start = number(reference.start, -1);
        var end = number(reference.end, -1);
        var url = safePullRequestUrl(reference.url);
        if (!Number.isInteger(start) || !Number.isInteger(end) || start < cursor ||
            end <= start || end > content.length || !url) {
          return;
        }
        container.appendChild(document.createTextNode(content.slice(cursor, start)));
        var link = htmlElement("a", "pr-reference", content.slice(start, end));
        link.href = url;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        link.title = text(
          reference.title,
          text(reference.repository, "GitHub") + " pull request #" + formatCount(reference.number)
        );
        if (text(reference.title)) {
          var state = reference.draft
            ? "Draft"
            : (text(reference.merged_at) ? "Merged" : text(reference.state, "Unknown state"));
          var branch = text(reference.base_ref) && text(reference.head_label)
            ? text(reference.base_ref) + " ← " + text(reference.head_label)
            : "";
          link.setAttribute(
            "aria-label",
            text(reference.repository, "GitHub") + " pull request #" +
              formatCount(reference.number) + ": " + text(reference.title)
          );
          link.addEventListener("pointerenter", function (event) {
            showTooltip(
              event,
              text(reference.title),
              text(reference.body_excerpt, "No pull request description available.") +
                (branch ? "\n\n" + branch : ""),
              state + " · " + text(reference.repository) +
                (text(reference.author) ? " · @" + text(reference.author) : "")
            );
          });
          link.addEventListener("pointermove", positionTooltip);
          link.addEventListener("pointerleave", hideTooltip);
        }
        container.appendChild(link);
        cursor = end;
    });
    container.appendChild(document.createTextNode(content.slice(cursor)));
    linkKnownGlossaryTerms(container);
    return container;
  }

  function svgElement(tag, attributes, content) {
    var element = document.createElementNS(SVG_NS, tag);
    if (attributes) {
      Object.keys(attributes).forEach(function (name) {
        element.setAttribute(name, String(attributes[name]));
      });
    }
    if (content !== undefined && content !== null) {
      element.textContent = String(content);
    }
    return element;
  }

  function clamp(value, minimum, maximum) {
    return Math.max(minimum, Math.min(maximum, value));
  }

  function rangesOverlap(startA, endA, startB, endB) {
    return endA >= startB && startA <= endB;
  }

  function normalizeKind(value, allowed, fallback) {
    var normalized = text(value).toLowerCase().replace(/[^a-z0-9_-]/g, "");
    return allowed.indexOf(normalized) >= 0 ? normalized : fallback;
  }

  function formatCount(value) {
    return Math.max(0, Number(value) || 0).toLocaleString();
  }

  function formatStatsInline(stats) {
    if (!stats || typeof stats !== "object") {
      return "";
    }
    var values = [
      formatCount(stats.user_prompts) + " prompts",
      formatCount(stats.agent_responses) + " responses",
      formatCount(stats.inter_agent_messages) + " inter-agent",
      formatCount(stats.tool_calls) + " tools"
    ];
    if (Number(stats.external_messages) > 0) {
      values.splice(3, 0, formatCount(stats.external_messages) + " external");
    }
    return values.join(" · ");
  }

  function installTimezone(value) {
    var candidate = text(value);
    if (!candidate) {
      app.timezone = "UTC";
      app.timezoneStatus = "archive timezone missing; using UTC";
      return;
    }
    try {
      new Intl.DateTimeFormat(undefined, { timeZone: candidate }).format(new Date());
      app.timezone = candidate;
      app.timezoneStatus = "archive";
    } catch (_error) {
      app.timezone = "UTC";
      app.timezoneStatus = "archive timezone unsupported; using UTC";
    }
  }

  function dateFormatter(options) {
    var settings = Object.assign({}, options);
    settings.timeZone = app.timezone;
    return new Intl.DateTimeFormat(undefined, settings);
  }

  function formatFullTime(milliseconds) {
    if (!Number.isFinite(milliseconds)) {
      return "Unknown time";
    }
    return dateFormatter({
      year: "numeric",
      month: "short",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      timeZoneName: "short"
    }).format(new Date(milliseconds));
  }

  function formatCompactTime(milliseconds) {
    return dateFormatter({
      month: "short",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit"
    }).format(new Date(milliseconds));
  }

  function formatTick(milliseconds, span) {
    var options;
    if (span <= 6 * 60 * 60 * 1000) {
      options = { hour: "2-digit", minute: "2-digit", second: "2-digit" };
    } else if (span <= 3 * 24 * 60 * 60 * 1000) {
      options = { weekday: "short", hour: "2-digit", minute: "2-digit" };
    } else if (span <= 120 * 24 * 60 * 60 * 1000) {
      options = { month: "short", day: "2-digit", hour: "2-digit" };
    } else {
      options = { year: "numeric", month: "short", day: "2-digit" };
    }
    return dateFormatter(options).format(new Date(milliseconds));
  }

  function formatRange(start, end) {
    return formatCompactTime(start) + " — " + formatCompactTime(end);
  }

  function normalizeData(raw) {
    if (!raw || typeof raw !== "object") {
      throw new Error("timeline.json must contain a JSON object");
    }
    var normalized = {
      schema_version: raw.schema_version,
      generated_at: text(raw.generated_at),
      source_digest: text(raw.source_digest),
      display_timezone: text(raw.display_timezone),
      display_timezone_source: text(raw.display_timezone_source, "legacy_team_data"),
      range: raw.range && typeof raw.range === "object" ? raw.range : {},
      teams: array(raw.teams),
      agents: array(raw.agents),
      phases: array(raw.phases),
      activity_bins: array(raw.activity_bins),
      edges: array(raw.edges),
      events: array(raw.events),
      rollups: array(raw.rollups),
      summary_files: array(raw.summary_files),
      glossary: array(raw.glossary),
      glossary_path: text(raw.glossary_path),
      artifact_catalog_path: text(raw.artifact_catalog_path),
      stats: raw.stats && typeof raw.stats === "object" ? raw.stats : null
    };

    var inferredStart = Infinity;
    var inferredEnd = -Infinity;
    function recordTimestamp(value) {
      var timestamp = number(value, NaN);
      if (Number.isFinite(timestamp)) {
        inferredStart = Math.min(inferredStart, timestamp);
        inferredEnd = Math.max(inferredEnd, timestamp);
      }
    }
    normalized.agents.forEach(function (agent) {
      recordTimestamp(agent.start_ms);
      recordTimestamp(agent.end_ms);
    });
    normalized.phases.forEach(function (phase) {
      recordTimestamp(phase.start_ms);
      recordTimestamp(phase.end_ms);
    });
    normalized.events.forEach(function (event) {
      recordTimestamp(event.at_ms);
    });
    if (!Number.isFinite(inferredStart) || !Number.isFinite(inferredEnd)) {
      inferredStart = Date.now() - 3600000;
      inferredEnd = Date.now();
    }
    var start = number(normalized.range.start_ms, inferredStart);
    var end = number(normalized.range.end_ms, inferredEnd);
    if (end <= start) {
      end = start + 1;
    }
    normalized.range = { start_ms: start, end_ms: end };
    return normalized;
  }

  function prepareArtifactCatalog(data) {
    app.artifactsById.clear();
    app.artifactCatalogError = "";
    app.artifactCatalogPromise = null;
    var path = text(data && data.artifact_catalog_path);
    if (!path) {
      app.artifactCatalogState = "legacy";
    } else {
      app.artifactCatalogState = "unloaded";
    }
  }

  async function ensureArtifactCatalog(data) {
    if (app.artifactCatalogState === "legacy" ||
        app.artifactCatalogState === "ready" ||
        app.artifactCatalogState === "error") {
      return;
    }
    if (app.artifactCatalogPromise) {
      return app.artifactCatalogPromise;
    }
    var path = text(data && data.artifact_catalog_path);
    if (!path) {
      app.artifactCatalogState = "legacy";
      return;
    }
    app.artifactCatalogState = "loading";
    app.artifactCatalogPromise = (async function () {
      try {
        var loaded = await fetchPath(path, "json");
        var catalog = loaded.content;
        if (!catalog || typeof catalog !== "object" ||
            number(catalog.schema_version, NaN) !== 1 ||
            !Array.isArray(catalog.artifacts)) {
          throw new Error("Artifact catalog is not a supported schema-1 catalog.");
        }
        catalog.artifacts.forEach(function (artifact) {
          if (!artifact || typeof artifact !== "object") {
            throw new Error("Artifact catalog entries must be objects.");
          }
          var id = text(artifact.artifact_id);
          if (!/^artifact-[a-z0-9-]+$/.test(id) || app.artifactsById.has(id)) {
            throw new Error("Artifact catalog contains an invalid or duplicate ID.");
          }
          app.artifactsById.set(id, artifact);
        });
        app.artifactCatalogState = "ready";
      } catch (error) {
        app.artifactsById.clear();
        app.artifactCatalogState = "error";
        app.artifactCatalogError = error instanceof Error ? error.message : String(error);
      }
    }());
    return app.artifactCatalogPromise;
  }

  function dataVersionLabel(value) {
    if (value === undefined || value === null || value === "") {
      return "schema unknown";
    }
    return "schema " + String(value);
  }

  function safeRepositoryUrl(value) {
    var raw = text(value);
    if (!raw) {
      return "";
    }
    try {
      var parsed = new URL(raw);
      return parsed.protocol === "https:" || parsed.protocol === "http:" ? parsed.href : "";
    } catch (_error) {
      return "";
    }
  }

  function identitySourceLabel(value) {
    var source = text(value);
    if (source === "explicit") {
      return "explicit";
    }
    if (source === "session_metadata") {
      return "from session metadata";
    }
    return source.replace(/_/g, " ");
  }

  function aggregatedSiteIdentity(teams) {
    var projectsByKey = new Map();
    var projectOrder = [];
    var hostsByKey = new Map();
    var hostOrder = [];
    teams.forEach(function (team) {
      if (!team || typeof team !== "object") {
        return;
      }
      array(team.projects).forEach(function (project) {
        if (!project || typeof project !== "object") {
          return;
        }
        var label = text(project.label);
        var repositoryUrl = safeRepositoryUrl(project.repository_url);
        if (!label) {
          return;
        }
        var key = repositoryUrl || "label:" + label.toLowerCase();
        if (!projectsByKey.has(key)) {
          projectOrder.push(key);
        }
        var prior = projectsByKey.get(key);
        projectsByKey.set(key, {
          label: label,
          repository_url: repositoryUrl,
          primary: project.primary === true || Boolean(prior && prior.primary),
          source: text(project.source)
        });
      });
      array(team.hosts).forEach(function (host) {
        if (!host || typeof host !== "object") {
          return;
        }
        var hostname = text(host.hostname);
        var key = hostname.toLowerCase();
        if (!hostname || hostsByKey.has(key)) {
          return;
        }
        hostOrder.push(key);
        hostsByKey.set(key, {
          hostname: hostname,
          source: text(host.source)
        });
      });
    });
    var projects = projectOrder.map(function (key) { return projectsByKey.get(key); });
    projects.sort(function (left, right) {
      return Number(right.primary) - Number(left.primary);
    });
    return {
      projects: projects,
      hosts: hostOrder.map(function (key) { return hostsByKey.get(key); })
    };
  }

  function projectNode(project) {
    if (project.repository_url) {
      var link = htmlElement("a", "", project.label);
      link.href = project.repository_url;
      link.target = "_blank";
      link.rel = "noopener noreferrer";
      return link;
    }
    return htmlElement("span", "", project.label);
  }

  function renderSiteIdentity(data) {
    var identity = aggregatedSiteIdentity(data.teams);
    var primaryProjects = identity.projects.filter(function (project) {
      return project.primary;
    });
    var titleProject = identity.projects.length === 1
      ? identity.projects[0]
      : (primaryProjects.length === 1 ? primaryProjects[0] : null);
    var projectSummary;
    var projectVisible;
    if (titleProject) {
      projectSummary = projectNode(titleProject);
      projectVisible = titleProject.label;
    } else if (identity.projects.length > 1) {
      projectSummary = htmlElement("span", "", "multi-repo");
      projectVisible = "multi-repo";
    } else {
      var fallback = data.teams.length === 1 && data.teams[0]
        ? text(data.teams[0].label, text(data.teams[0].slug, "unknown project"))
        : (data.teams.length > 1 ? "multi-team" : "unknown project");
      projectSummary = htmlElement("span", "", fallback);
      projectVisible = fallback;
    }
    var children = [document.createTextNode("Agent Timeline: "), projectSummary];
    var hostVisible = "";
    if (identity.hosts.length === 1) {
      hostVisible = identity.hosts[0].hostname.split(".")[0];
    } else if (identity.hosts.length > 1) {
      hostVisible = "multi-host";
    }
    if (hostVisible) {
      children.push(document.createTextNode(", " + hostVisible));
    }
    dom.siteTitle.replaceChildren.apply(dom.siteTitle, children);
    var visibleTitle = "Agent Timeline: " + projectVisible + (hostVisible ? ", " + hostVisible : "");
    document.title = visibleTitle;

    var fullLines = [];
    if (identity.projects.length) {
      fullLines.push(
        "Projects: " + identity.projects.map(function (project) {
          return project.label + (project.repository_url ? " (" + project.repository_url + ")" : "");
        }).join(", ")
      );
    }
    if (identity.hosts.length) {
      fullLines.push("Hosts: " + identity.hosts.map(function (host) {
        return host.hostname;
      }).join(", "));
    }
    dom.siteTitle.title = fullLines.join("\n");
    dom.siteTitle.setAttribute("aria-label", visibleTitle + (fullLines.length ? ". " + fullLines.join(". ") : ""));

    var detailChildren = [];
    if (identity.projects.length) {
      detailChildren.push(htmlElement("h2", "", "Projects / repositories"));
      var projectList = htmlElement("ul");
      identity.projects.forEach(function (project) {
        var item = htmlElement("li");
        item.appendChild(projectNode(project));
        var source = identitySourceLabel(project.source);
        if (source) {
          item.appendChild(htmlElement("span", "site-identity-source", "(" + source + ")"));
        }
        projectList.appendChild(item);
      });
      detailChildren.push(projectList);
    }
    if (identity.hosts.length) {
      detailChildren.push(htmlElement("h2", "", "Execution hosts"));
      var hostList = htmlElement("ul");
      identity.hosts.forEach(function (host) {
        var item = htmlElement("li", "", host.hostname);
        var source = identitySourceLabel(host.source);
        if (source) {
          item.appendChild(htmlElement("span", "site-identity-source", "(" + source + ")"));
        }
        hostList.appendChild(item);
      });
      detailChildren.push(hostList);
    }
    dom.identityList.replaceChildren.apply(dom.identityList, detailChildren);
    dom.identityDetails.hidden = detailChildren.length === 0;
  }

  function indexPhase(phase) {
    if (!phase || typeof phase !== "object") {
      return;
    }
    var id = text(phase.agent_id);
    if (!app.phasesByAgent.has(id)) {
      app.phasesByAgent.set(id, []);
    }
    app.phasesByAgent.get(id).push(phase);
  }

  function sortPhaseIndexes() {
    app.phasesByAgent.forEach(function (phases) {
      phases.sort(function (left, right) {
        return number(left.start_ms, 0) - number(right.start_ms, 0) ||
          text(left.id).localeCompare(text(right.id));
      });
    });
  }

  function updateShardDiagnostics() {
    dom.card.dataset.timelineSchemaMode = app.schemaMode;
    dom.card.dataset.detailShardCount = String(app.shardCatalog.length);
    dom.card.dataset.loadedShardCount = String(app.loadedShardUrls.size);
    dom.card.dataset.searchShardState = app.searchShardState;
    dom.card.dataset.transcriptSearchState = app.transcriptSearchState;
    dom.card.dataset.loadedSearchShardCount = String(app.loadedSearchShardUrls.size);
    dom.card.dataset.transcriptSearchResultCount = String(app.transcriptSearchTotal);
    dom.card.dataset.phaseIndexState = app.phaseIndexReady
      ? "ready"
      : (app.phaseIndexReference || schema3Enabled()
        ? (app.phaseIndexPromise || app.phaseCardPromises.size ? "requested" : "unloaded")
        : "legacy");
  }

  function initializeData(raw) {
    var data = normalizeData(raw);
    app.data = data;
    prepareArtifactCatalog(data);
    installTimezone(data.display_timezone);
    app.navigationRange = timelineCore
      ? timelineCore.navigableRange(data.range, data.rollups)
      : data.range;
    app.viewStart = data.range.start_ms;
    app.viewEnd = data.range.end_ms;
    app.agentsById.clear();
    app.teamBySlug.clear();
    app.phasesByAgent.clear();
    app.glossaryById.clear();
    app.phaseIds.clear();
    app.edgeIds.clear();

    data.teams.forEach(function (team) {
      if (team && typeof team === "object") {
        app.teamBySlug.set(text(team.slug), team);
      }
    });
    data.agents.forEach(function (agent) {
      if (!agent || typeof agent !== "object") {
        return;
      }
      var id = text(agent.id);
      if (id) {
        app.agentsById.set(id, agent);
      }
    });
    data.phases.forEach(function (phase) {
      if (!phase || typeof phase !== "object") {
        return;
      }
      var phaseId = text(phase.id);
      if (phaseId) {
        app.phaseIds.add(phaseId);
      }
      indexPhase(phase);
    });
    sortPhaseIndexes();
    data.edges.forEach(function (edge) {
      var edgeId = text(edge && edge.id);
      if (edgeId) {
        app.edgeIds.add(edgeId);
      }
    });
    data.glossary.forEach(function (entry) {
      if (!entry || typeof entry !== "object") {
        throw new Error("glossary entries must be objects");
      }
      var id = glossaryId(entry.id);
      var term = text(entry.term);
      if (!id || !term || text(entry.url) !== "#glossary/" + id) {
        throw new Error("glossary entry has an invalid stable target");
      }
      if (app.glossaryById.has(id)) {
        throw new Error("glossary contains a duplicate ID");
      }
      app.glossaryById.set(id, entry);
    });
    rebuildTeamActivityScores();
    rebuildSummaryRanges();
    dom.glossaryOpen.hidden = !data.glossary.length && !data.glossary_path;

    renderSiteIdentity(data);
    populateTeamFilter();
    populateSummaryFiles();
    var generated = data.generated_at ? "generated " + data.generated_at : "generation time unknown";
    var timezone = app.timezone;
    var timezoneSource = text(data.display_timezone_source).replace(/_/g, " ");
    dom.meta.textContent =
      dataVersionLabel(data.schema_version) + " · " + generated + " · display " + timezone +
      (timezoneSource ? " (" + timezoneSource + ")" : "") +
      (app.timezoneStatus === "archive" ? "" : " · " + app.timezoneStatus);
    updateShardDiagnostics();
    scheduleRender();
    openGlossaryFromHash();
  }

  function immutableTimelineObjectUrl(reference, where) {
    if (!reference || typeof reference !== "object") {
      throw new Error(where + " must be an object reference.");
    }
    var url = text(reference.url);
    var digest = text(reference.sha256);
    var match = /^data\/timeline-v2\/objects\/([0-9a-f]{64})\.json$/.exec(url);
    if (!match || match[1] !== digest) {
      throw new Error(where + " must use its complete SHA-256 object URL.");
    }
    return url;
  }

  function validateSchema2Generation(bootstrap, globalData) {
    var sourceDigest = text(bootstrap.source_digest);
    if (!sourceDigest) {
      throw new Error("Schema-2 timeline bootstrap is missing its source digest.");
    }
    // Schema-2 globals published before generation binding was introduced do not carry this
    // field.  Keep those immutable archives readable; every newly generated global does carry
    // it, and a present value must bind exactly to the bootstrap before any data is installed.
    if (hasField(globalData, "source_digest") &&
        text(globalData.source_digest) !== sourceDigest) {
      throw new Error("Schema-2 timeline global source digest does not match its bootstrap.");
    }

    var bootstrapRange = bootstrap.range && typeof bootstrap.range === "object"
      ? bootstrap.range
      : null;
    var rangeStart = number(bootstrapRange && bootstrapRange.start_ms, NaN);
    var rangeEnd = number(bootstrapRange && bootstrapRange.end_ms, NaN);
    if (!Number.isFinite(rangeStart) || !Number.isFinite(rangeEnd) || rangeEnd <= rangeStart) {
      throw new Error("Schema-2 timeline bootstrap has an invalid range.");
    }
    if (hasField(globalData, "range")) {
      var globalRange = globalData.range && typeof globalData.range === "object"
        ? globalData.range
        : null;
      if (number(globalRange && globalRange.start_ms, NaN) !== rangeStart ||
          number(globalRange && globalRange.end_ms, NaN) !== rangeEnd) {
        throw new Error("Schema-2 timeline global range does not match its bootstrap.");
      }
    }

    var teamSlugs = new Set();
    array(bootstrap.teams).forEach(function (team) {
      var slug = text(team && team.slug);
      if (!slug || teamSlugs.has(slug)) {
        throw new Error("Schema-2 timeline bootstrap has an invalid or duplicate team.");
      }
      teamSlugs.add(slug);
    });
    if (!teamSlugs.size) {
      throw new Error("Schema-2 timeline bootstrap does not identify a team.");
    }
    if (hasField(globalData, "teams")) {
      var globalTeamSlugs = new Set();
      array(globalData.teams).forEach(function (team) {
        globalTeamSlugs.add(text(team && team.slug));
      });
      if (globalTeamSlugs.size !== teamSlugs.size ||
          Array.from(globalTeamSlugs).some(function (slug) { return !teamSlugs.has(slug); })) {
        throw new Error("Schema-2 timeline global teams do not match its bootstrap.");
      }
    }

    [
      "agents",
      "edges",
      "rollups",
      "summary_files",
      "project_overviews",
      "projects",
      "glossary"
    ].forEach(function (field) {
      array(globalData[field]).forEach(function (record) {
        var team = text(record && record.team);
        if (team && !teamSlugs.has(team)) {
          throw new Error("Schema-2 timeline global " + field +
            " contains a team absent from its bootstrap.");
        }
      });
    });
    array(globalData.agents).forEach(function (agent) {
      var start = number(agent && agent.start_ms, NaN);
      var end = number(agent && agent.end_ms, NaN);
      if (Number.isFinite(start) && Number.isFinite(end) &&
          (start < rangeStart || end > rangeEnd)) {
        throw new Error("Schema-2 timeline global agent range is outside its bootstrap.");
      }
    });
  }

  function validateSchema2ObjectSourceDigest(raw, where) {
    if (!hasField(raw, "source_digest")) {
      return;
    }
    var expected = text(app.data && app.data.source_digest);
    if (!expected || text(raw.source_digest) !== expected) {
      throw new Error(where + " source digest does not match the timeline generation.");
    }
  }

  function mergeDetailShard(raw, url) {
    if (!raw || typeof raw !== "object" || number(raw.schema_version, NaN) !== 2 ||
        text(raw.kind) !== "timeline-detail-day") {
      throw new Error("Unsupported timeline detail shard: " + url);
    }
    if (app.loadedShardUrls.has(url)) {
      return;
    }
    var phases = array(raw.phases);
    var edges = array(raw.edges);
    var events = array(raw.events);
    var shardPhaseIds = new Set();
    var shardEdgeIds = new Set();
    phases.forEach(function (phase) {
      if (!phase || typeof phase !== "object") {
        throw new Error("Timeline detail phases must be objects.");
      }
      var id = text(phase.id);
      if (!id || shardPhaseIds.has(id)) {
        throw new Error("Timeline detail phase has a missing or duplicate stable ID.");
      }
      shardPhaseIds.add(id);
    });
    edges.forEach(function (edge) {
      if (!edge || typeof edge !== "object") {
        throw new Error("Timeline detail edges must be objects.");
      }
      var id = text(edge.id);
      if (!id || shardEdgeIds.has(id)) {
        throw new Error("Timeline detail edge has a missing or duplicate stable ID.");
      }
      shardEdgeIds.add(id);
    });
    events.forEach(function (event) {
      if (!event || typeof event !== "object") {
        throw new Error("Timeline detail events must be objects.");
      }
    });

    // Validation above is deliberately atomic: a failed response must not leave partial phases,
    // edges, or id-less events that a later retry would duplicate.
    phases.forEach(function (phase) {
      var id = text(phase.id);
      if (!app.phaseIds.has(id)) {
        app.phaseIds.add(id);
        app.data.phases.push(phase);
        indexPhase(phase);
      }
    });
    edges.forEach(function (edge) {
      var id = text(edge.id);
      if (!app.edgeIds.has(id)) {
        app.edgeIds.add(id);
        app.data.edges.push(edge);
      }
    });
    // Events belong to exactly one UTC-day object, so URL-level application is sufficient to
    // retain legitimate duplicate events while rejecting repeat fetch/application.
    events.forEach(function (event) {
      app.data.events.push(event);
    });
    sortPhaseIndexes();
    app.data.phases.sort(function (left, right) {
      return number(left.start_ms, 0) - number(right.start_ms, 0) ||
        text(left.id).localeCompare(text(right.id));
    });
    app.data.edges.sort(function (left, right) {
      return number(left.source_ms, 0) - number(right.source_ms, 0) ||
        text(left.id).localeCompare(text(right.id));
    });
    app.data.events.sort(function (left, right) {
      return number(left.at_ms, 0) - number(right.at_ms, 0) ||
        text(left.agent_id).localeCompare(text(right.agent_id));
    });
    rebuildSummaryRanges();
    app.loadedShardUrls.add(url);
    if (app.detailErrorActive) {
      app.detailErrorActive = false;
      dom.loadError.hidden = true;
      dom.loadError.textContent = "";
    }
    updateShardDiagnostics();
    scheduleRender();
  }

  function loadDetailShard(catalogEntry) {
    if (schema3Enabled()) {
      return loadSchema3DetailShard(catalogEntry);
    }
    var url = immutableTimelineObjectUrl(catalogEntry, "detail shard");
    if (!app.detailPromises.has(url)) {
      var request = fetchContentAddressedJson(catalogEntry, "detail shard").then(function (raw) {
        mergeDetailShard(raw, url);
        return raw;
      });
      var cached = request.catch(function (error) {
        if (app.detailPromises.get(url) === cached) {
          app.detailPromises.delete(url);
        }
        throw error;
      });
      app.detailPromises.set(url, cached);
    }
    return app.detailPromises.get(url);
  }

  function detailShardsForRange(start, end, bufferFraction) {
    var span = Math.max(1, end - start);
    var buffer = Math.max(60 * 60 * 1000, span * bufferFraction);
    var bufferedStart = start - buffer;
    var bufferedEnd = end + buffer;
    return app.shardCatalog.filter(function (shard) {
      return number(shard.start_ms, Infinity) < bufferedEnd &&
        number(shard.end_ms, -Infinity) > bufferedStart;
    });
  }

  function requestDetailShards(shards) {
    var started = false;
    var promises = shards.map(function (shard) {
      var url = shardKey(shard, "detail shard");
      if (!app.detailPromises.has(url)) {
        started = true;
      }
      return loadDetailShard(shard);
    });
    var promise = Promise.all(promises);
    if (started) {
      promise.then(scheduleRender, function () { return undefined; });
    }
    return { started: started, promise: promise };
  }

  function showDetailLoadError(error) {
    var message = error instanceof Error ? error.message : String(error);
    dom.loadError.textContent =
      "Could not load timeline detail: " + message +
      ". The aggregate timeline remains available.";
    dom.loadError.hidden = false;
    app.detailErrorActive = true;
  }

  function requestVisibleDetails() {
    if (!shardedMode() || app.renderLod !== "detail") {
      return;
    }
    var request = requestDetailShards(
      detailShardsForRange(app.viewStart, app.viewEnd, 0.08)
    );
    request.promise.catch(showDetailLoadError);
  }

  function requestSearchCorpus() {
    if (!shardedMode()) {
      return;
    }
    app.searchShardState = "loading";
    updateShardDiagnostics();
    var request = requestDetailShards(app.shardCatalog);
    request.promise.then(function () {
      app.searchShardState = "ready";
      updateShardDiagnostics();
      scheduleRender();
    }).catch(function (error) {
      app.searchShardState = "error";
      updateShardDiagnostics();
      showDetailLoadError(error);
    });
  }

  function asciiLowerUtf8SearchBytes(value) {
    var encoded = new TextEncoder().encode(compactSearchText(value));
    var lowered = new Uint8Array(encoded.length);
    encoded.forEach(function (byte, index) {
      lowered[index] = byte >= 65 && byte <= 90 ? byte + 32 : byte;
    });
    return lowered;
  }

  function asciiLowerSearchText(value) {
    return compactSearchText(value).replace(/[A-Z]/g, function (character) {
      return character.toLowerCase();
    });
  }

  function searchTextTrigrams(value) {
    var encoded = asciiLowerUtf8SearchBytes(value);
    if (encoded.length < 3) {
      return [];
    }
    var unique = new Map();
    for (var index = 0; index <= encoded.length - 3; index += 1) {
      var trigram = new Uint8Array([
        encoded[index], encoded[index + 1], encoded[index + 2]
      ]);
      unique.set(Array.from(trigram).join(","), trigram);
    }
    return Array.from(unique.values()).sort(function (left, right) {
      return left[0] - right[0] || left[1] - right[1] || left[2] - right[2];
    });
  }

  function queryTermBloomEligible(value) {
    var compact = compactSearchText(value);
    for (var index = 0; index < compact.length; index += 1) {
      if (compact.charCodeAt(index) > 127) {
        return false;
      }
    }
    return asciiLowerUtf8SearchBytes(compact).length >= 3;
  }

  function fnv1a32(value, seed) {
    var digest = seed >>> 0;
    value.forEach(function (byte) {
      digest = Math.imul((digest ^ byte) >>> 0, FNV_PRIME) >>> 0;
    });
    return digest;
  }

  function bloomBitPositions(trigram, bitCount, hashCount) {
    var first = fnv1a32(trigram, FNV_OFFSET);
    var second = (fnv1a32(
      trigram,
      (FNV_OFFSET ^ SECOND_HASH_SEED) >>> 0
    ) | 1) >>> 0;
    var positions = [];
    for (var index = 0; index < hashCount; index += 1) {
      positions.push((first + index * second) % bitCount);
    }
    return positions;
  }

  function decodeBase64Bytes(value, where) {
    var encoded = text(value);
    var valid = encoded.length % 4 === 0 &&
      /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(encoded);
    if (!valid) {
      throw new Error(where + ".bits_base64: invalid base64");
    }
    var decoded;
    try {
      decoded = atob(encoded);
    } catch (_error) {
      throw new Error(where + ".bits_base64: invalid base64");
    }
    var bytes = new Uint8Array(decoded.length);
    for (var index = 0; index < decoded.length; index += 1) {
      bytes[index] = decoded.charCodeAt(index);
    }
    return bytes;
  }

  function decodeTrigramBloom(value, where) {
    if (!value || typeof value !== "object" ||
        text(value.algorithm) !== TRIGRAM_BLOOM_ALGORITHM) {
      throw new Error(where + ".algorithm: unsupported trigram Bloom filter");
    }
    var bitCount = number(value.bit_count, NaN);
    var hashCount = number(value.hash_count, NaN);
    var trigramCount = number(value.trigram_count, NaN);
    if (!Number.isSafeInteger(bitCount) || bitCount < 64 ||
        !Number.isInteger(Math.log2(bitCount))) {
      throw new Error(where + ".bit_count: expected a power of two of at least 64");
    }
    if (hashCount !== TRIGRAM_BLOOM_HASH_COUNT) {
      throw new Error(
        where + ".hash_count: expected " + TRIGRAM_BLOOM_HASH_COUNT
      );
    }
    if (!Number.isSafeInteger(trigramCount) || trigramCount < 0) {
      throw new Error(where + ".trigram_count: expected a non-negative integer");
    }
    var bits = decodeBase64Bytes(value.bits_base64, where);
    if (bits.length * 8 !== bitCount) {
      throw new Error(
        where + ".bits_base64: decoded " + bits.length * 8 +
        " bits, expected " + bitCount
      );
    }
    return {
      bitCount: bitCount,
      hashCount: hashCount,
      bits: bits,
      trigramCount: trigramCount
    };
  }

  function trigramBloomMightContain(filterValue, queryTerm) {
    if (!queryTermBloomEligible(queryTerm)) {
      return true;
    }
    return searchTextTrigrams(queryTerm).every(function (trigram) {
      return bloomBitPositions(
        trigram,
        filterValue.bitCount,
        filterValue.hashCount
      ).every(function (position) {
        return Boolean(
          filterValue.bits[Math.floor(position / 8)] & (1 << (position % 8))
        );
      });
    });
  }

  function trigramBloomMightMatchQuery(filterValue, query) {
    var parts = searchQueryParts(query);
    var eligible = parts.filter(function (part) {
      return queryTermBloomEligible(part.value);
    });
    if (!eligible.length) {
      return true;
    }
    return eligible.every(function (part) {
      return trigramBloomMightContain(filterValue, part.value);
    });
  }

  function searchShardMightMatch(shard, query) {
    var url = shardKey(shard, "transcript search shard");
    var filterValue = app.searchBloomByUrl.get(url);
    if (!filterValue) {
      return true;
    }
    return trigramBloomMightMatchQuery(filterValue, query);
  }

  function transcriptSearchRecordCount(catalogEntry) {
    var counts = catalogEntry && typeof catalogEntry.counts === "object"
      ? catalogEntry.counts
      : null;
    var count = number(counts && counts.records, NaN);
    return Number.isInteger(count) && count >= 0 ? count : NaN;
  }

  function validateTranscriptSearchShard(raw, catalogEntry, url) {
    if (!raw || typeof raw !== "object" || number(raw.schema_version, NaN) !== 1 ||
        text(raw.kind) !== "timeline-search-day") {
      throw new Error("Unsupported transcript search shard: " + url);
    }
    validateSchema2ObjectSourceDigest(raw, "Transcript search shard");
    var expectedTeam = text(catalogEntry && catalogEntry.team);
    var expectedStart = number(catalogEntry && catalogEntry.start_ms, NaN);
    var expectedEnd = number(catalogEntry && catalogEntry.end_ms, NaN);
    var expectedCount = transcriptSearchRecordCount(catalogEntry);
    var range = raw.range && typeof raw.range === "object" ? raw.range : null;
    var actualStart = number(range && range.start_ms, NaN);
    var actualEnd = number(range && range.end_ms, NaN);
    if (!expectedTeam || text(raw.team) !== expectedTeam ||
        !Number.isFinite(expectedStart) || !Number.isFinite(expectedEnd) ||
        actualStart !== expectedStart || actualEnd !== expectedEnd ||
        !Number.isFinite(expectedCount) || !Array.isArray(raw.records) ||
        raw.records.length !== expectedCount) {
      throw new Error("Transcript search shard does not match its catalog entry: " + url);
    }
    var allowedRoles = [
      "user", "assistant", "agent", "system", "external", "goal", "tool", "event"
    ];
    raw.records.forEach(function (record) {
      if (!record || typeof record !== "object" ||
          number(record.schema_version, NaN) !== 1) {
        throw new Error("Transcript search records must be schema-1 objects.");
      }
      var reference = text(record.ref);
      var recordType = text(record.record_type);
      var expectedPrefix = (recordType === "tool" ? "tool:" : "message:") +
        expectedTeam + "::";
      var agentId = text(record.agent_id);
      var localAgentId = agentId.startsWith(expectedTeam + "::")
        ? agentId.slice(expectedTeam.length + 2)
        : agentId;
      var expectedAgentReference = "agent:" + expectedTeam + "::" + localAgentId;
      var agent = app.agentsById.get(agentId);
      var role = text(record.role);
      var at = number(record.at_ms, NaN);
      var promptReference = text(record.prompt_ref);
      var eventId = text(record.event_id);
      if (text(record.team) !== expectedTeam || !reference.startsWith(expectedPrefix) ||
          reference.length <= expectedPrefix.length || !eventId ||
          reference !== expectedPrefix + eventId || !agentId || !agent ||
          text(record.agent_ref) !== expectedAgentReference ||
          text(agent.team) !== expectedTeam || allowedRoles.indexOf(role) < 0 ||
          !text(record.text) || !Number.isFinite(at) ||
          at < expectedStart || at >= expectedEnd ||
          (promptReference && !promptReference.startsWith("message:" + expectedTeam + "::"))) {
        throw new Error("Transcript search shard has an invalid record: " + url);
      }
    });
    return raw.records;
  }

  function mergeTranscriptSearchShard(raw, catalogEntry, url) {
    if (app.loadedSearchShardUrls.has(url)) {
      return;
    }
    var records = validateTranscriptSearchShard(raw, catalogEntry, url);
    var pending = [];
    var localRefs = new Set();
    records.forEach(function (record) {
      var reference = text(record.ref);
      if (localRefs.has(reference) ||
          app.searchRecordsByRef.has(reference)) {
        throw new Error("Transcript search shard has an invalid or duplicate record.");
      }
      localRefs.add(reference);
      pending.push(record);
    });
    pending.forEach(function (record) {
      app.searchRecords.push(record);
      app.searchRecordsByRef.set(text(record.ref), record);
    });
    app.searchRecords.sort(function (left, right) {
      return number(left.at_ms, 0) - number(right.at_ms, 0) ||
        text(left.ref).localeCompare(text(right.ref));
    });
    app.loadedSearchShardUrls.add(url);
    updateShardDiagnostics();
  }

  function loadTranscriptSearchShard(catalogEntry) {
    if (app.searchCorpusMode === "schema3") {
      return loadSchema3SearchShard(catalogEntry);
    }
    var url = immutableTimelineObjectUrl(catalogEntry, "transcript search shard");
    return fetchContentAddressedJson(
      catalogEntry,
      "transcript search shard"
    ).then(function (raw) {
      mergeTranscriptSearchShard(raw, catalogEntry, url);
      return raw;
    });
  }

  function validateTranscriptSearchLinkage(raw, catalogEntry, url) {
    if (!raw || typeof raw !== "object" || number(raw.schema_version, NaN) !== 1 ||
        text(raw.kind) !== "timeline-search-links-day") {
      throw new Error("Unsupported transcript search linkage shard: " + url);
    }
    validateSchema2ObjectSourceDigest(raw, "Transcript search linkage shard");
    var expectedTeam = text(catalogEntry && catalogEntry.team);
    var expectedStart = number(catalogEntry && catalogEntry.start_ms, NaN);
    var expectedEnd = number(catalogEntry && catalogEntry.end_ms, NaN);
    var range = raw.range && typeof raw.range === "object" ? raw.range : null;
    var linkage = catalogEntry && catalogEntry.linkage &&
      typeof catalogEntry.linkage === "object" ? catalogEntry.linkage : null;
    var counts = linkage && linkage.counts && typeof linkage.counts === "object"
      ? linkage.counts
      : null;
    var prompts = array(raw.prompts);
    var responses = array(raw.responses);
    if (!expectedTeam || text(raw.team) !== expectedTeam ||
        number(range && range.start_ms, NaN) !== expectedStart ||
        number(range && range.end_ms, NaN) !== expectedEnd ||
        number(counts && counts.prompts, NaN) !== prompts.length ||
        number(counts && counts.responses, NaN) !== responses.length) {
      throw new Error(
        "Transcript search linkage shard does not match its catalog entry: " + url
      );
    }
    var messagePrefix = "message:" + expectedTeam + "::";
    var agentPrefix = "agent:" + expectedTeam + "::";
    prompts.forEach(function (prompt) {
      if (!prompt || typeof prompt !== "object" ||
          !text(prompt.ref).startsWith(messagePrefix) ||
          typeof prompt.excerpt !== "string") {
        throw new Error("Transcript search linkage shard has an invalid prompt: " + url);
      }
    });
    responses.forEach(function (response) {
      var at = number(response && response.at_ms, NaN);
      if (!response || typeof response !== "object" ||
          !text(response.ref).startsWith(messagePrefix) ||
          !text(response.prompt_ref).startsWith(messagePrefix) ||
          !text(response.agent_ref).startsWith(agentPrefix) ||
          !Number.isFinite(at) || at < expectedStart || at >= expectedEnd) {
        throw new Error("Transcript search linkage shard has an invalid response: " + url);
      }
    });
    return { prompts: prompts, responses: responses };
  }

  function mergeTranscriptSearchLinkage(raw, catalogEntry, url) {
    if (app.loadedSearchLinkUrls.has(url)) {
      return;
    }
    var linkage = validateTranscriptSearchLinkage(raw, catalogEntry, url);
    linkage.prompts.forEach(function (prompt) {
      var reference = text(prompt.ref);
      if (app.searchLinkPromptRefs.has(reference)) {
        throw new Error("Duplicate transcript search linkage prompt: " + reference);
      }
      app.searchLinkPromptRefs.add(reference);
      app.searchPromptExcerpts.set(reference, text(prompt.excerpt));
    });
    linkage.responses.forEach(function (response) {
      var reference = text(response.ref);
      var promptReference = text(response.prompt_ref);
      if (app.searchLinkResponseRefs.has(reference)) {
        throw new Error("Duplicate transcript search linkage response: " + reference);
      }
      app.searchLinkResponseRefs.add(reference);
      if (!app.searchResponsesByPrompt.has(promptReference)) {
        app.searchResponsesByPrompt.set(promptReference, []);
      }
      app.searchResponsesByPrompt.get(promptReference).push(response);
    });
    app.searchResponsesByPrompt.forEach(function (responses) {
      responses.sort(function (left, right) {
        return number(left.at_ms, 0) - number(right.at_ms, 0) ||
          text(left.ref).localeCompare(text(right.ref));
      });
    });
    app.loadedSearchLinkUrls.add(url);
  }

  function loadTranscriptSearchLinkage(catalogEntry) {
    var reference = catalogEntry && catalogEntry.linkage;
    if (!reference || typeof reference !== "object") {
      return Promise.resolve(null);
    }
    var url = immutableTimelineObjectUrl(reference, "transcript search linkage shard");
    return fetchContentAddressedJson(
      reference,
      "transcript search linkage shard"
    ).then(function (raw) {
      mergeTranscriptSearchLinkage(raw, catalogEntry, url);
      return raw;
    });
  }

  function loadSearchItemsBounded(items, loader, generation, isCurrent) {
    function pump() {
      while (app.searchLoadActive < SEARCH_LOAD_CONCURRENCY &&
             app.searchLoadQueue.length) {
        var job = app.searchLoadQueue.shift();
        if (job.generation !== null &&
            job.generation !== app.searchRequestGeneration) {
          job.resolve(null);
          continue;
        }
        if (job.isCurrent && !job.isCurrent()) {
          job.resolve(null);
          continue;
        }
        app.searchLoadActive += 1;
        Promise.resolve().then(job.run).then(job.resolve, job.reject).finally(
          function () {
            app.searchLoadActive -= 1;
            pump();
          }
        );
      }
    }
    return Promise.all(items.map(function (item) {
      return new Promise(function (resolve, reject) {
        app.searchLoadQueue.push({
          generation: generation,
          isCurrent: isCurrent || null,
          run: function () { return loader(item); },
          resolve: resolve,
          reject: reject
        });
        pump();
      });
    }));
  }

  function transcriptSearchShards() {
    return app.searchCatalog.filter(function (shard) {
      return (!app.selectedTeam || text(shard.team) === app.selectedTeam) &&
        searchShardMightMatch(shard, app.query);
    });
  }

  //: Load the prefilter for the teams in scope, but only when the query has a term a trigram can
  //: be built from.
  //:
  //: This is the whole difference the prefilter's move out of the bootstrap bought. Under schema 2
  //: the filters arrived whether or not they could be used; here `B3` -- the case study's own
  //: acceptance query, two bytes, ineligible for a trigram -- reads no Bloom data at all, and a
  //: `--team`-narrowed search reads one team's filters instead of the archive's.
  function prepareTranscriptSearchPrefilter() {
    if (app.searchCorpusMode !== "schema3" || !queryCanUseBloom(app.query)) {
      return Promise.resolve([]);
    }
    return Promise.all(schema3SearchTeams().map(ensureSearchBlooms));
  }

  function requestTranscriptSearchCorpus() {
    if (!shardedMode() || !app.searchCatalog.length) {
      app.transcriptSearchState = "unavailable";
      app.transcriptSearchError = "This export does not contain transcript search shards.";
      updateShardDiagnostics();
      renderTranscriptSearchResults();
      return Promise.resolve([]);
    }
    app.transcriptSearchState = "loading";
    app.transcriptSearchError = "";
    updateShardDiagnostics();
    renderTranscriptSearchResults();
    var generation = app.searchRequestGeneration;
    return prepareTranscriptSearchPrefilter().then(function () {
      if (generation !== app.searchRequestGeneration) {
        return [];
      }
      return requestTranscriptSearchShards(generation);
    }).catch(reportTranscriptSearchFailure(generation));
  }

  //: One failure handler for both steps of a search, so a prefilter that could not be fetched and
  //: a shard that could not be parsed leave the page in the same state and say so the same way.
  function reportTranscriptSearchFailure(generation) {
    return function (error) {
      if (generation !== app.searchRequestGeneration) {
        return [];
      }
      if (app.transcriptSearchState !== "error") {
        app.transcriptSearchState = "error";
        app.transcriptSearchError = errorMessage(error);
        updateShardDiagnostics();
        renderTranscriptSearchResults();
      }
      throw error;
    };
  }

  function requestTranscriptSearchShards(generation) {
    // Selected *after* the prefilter is in hand, which is the point of the two-step: a shard the
    // filter rules out is never fetched, and under schema 3 the filter itself is a fetch.
    var textShards = transcriptSearchShards();
    var linkage = app.searchCorpusMode === "schema3"
      ? loadSearchItemsBounded(schema3SearchTeams(), ensureSearchLinks, generation)
      : loadSearchItemsBounded(
          app.searchCatalog.filter(function (shard) {
            return !app.selectedTeam || text(shard.team) === app.selectedTeam;
          }),
          loadTranscriptSearchLinkage,
          generation
        );
    return Promise.all([
      loadSearchItemsBounded(textShards, loadTranscriptSearchShard, generation),
      linkage
    ]).then(function (values) {
      if (generation !== app.searchRequestGeneration) {
        return values;
      }
      app.transcriptSearchState = "ready";
      updateShardDiagnostics();
      updateTranscriptSearch();
      return values;
    }).catch(reportTranscriptSearchFailure(generation));
  }

  function transcriptRecordMatchesScope(record) {
    var recordType = text(record.record_type);
    if (app.searchScope === "owner-prompts") {
      return recordType === "prompt" && text(record.author_kind) === "owner_human";
    }
    if (app.searchScope === "agent-responses") {
      return recordType === "response" || recordType === "inter_agent_response";
    }
    return app.searchScope === "all-transcript";
  }

  function updateTranscriptSearch() {
    if (!transcriptSearchActive() || !app.query) {
      app.transcriptSearchResults = [];
      app.transcriptSearchTotal = 0;
      app.transcriptMatchedAgentIds.clear();
      app.activeSearchRef = "";
      renderTranscriptSearchResults();
      scheduleRender();
      return;
    }
    if (app.transcriptSearchState !== "ready") {
      renderTranscriptSearchResults();
      scheduleRender();
      return;
    }
    var matches = [];
    var matchedAgents = new Set();
    app.searchRecords.forEach(function (record) {
      if (!transcriptRecordMatchesScope(record) || !selectedTeamAllows(record)) {
        return;
      }
      var match = smartSearchMatch(record.text, app.query);
      if (!match) {
        return;
      }
      var agentId = text(record.agent_id);
      if (agentId) {
        matchedAgents.add(agentId);
      }
      matches.push({
        record: record,
        match: match,
        excerpt: searchExcerpt(match)
      });
    });
    if (app.searchSort === "newest") {
      matches.sort(function (left, right) {
        return number(right.record.at_ms, 0) - number(left.record.at_ms, 0) ||
          right.match.score - left.match.score ||
          text(left.record.ref).localeCompare(text(right.record.ref));
      });
    } else {
      matches.sort(function (left, right) {
        return right.match.score - left.match.score ||
          number(right.record.at_ms, 0) - number(left.record.at_ms, 0) ||
          text(left.record.ref).localeCompare(text(right.record.ref));
      });
    }
    app.transcriptSearchTotal = matches.length;
    app.transcriptSearchResults = matches.slice(0, SEARCH_RESULT_LIMIT);
    app.transcriptMatchedAgentIds = matchedAgents;
    if (app.activeSearchRef && !matches.some(function (item) {
      return text(item.record.ref) === app.activeSearchRef;
    })) {
      app.activeSearchRef = "";
    }
    renderTranscriptSearchResults();
    updateShardDiagnostics();
    scheduleRender();
  }

  function searchScopeTitle() {
    return {
      "owner-prompts": "My prompts",
      "agent-responses": "Agent responses",
      "all-transcript": "All transcript"
    }[app.searchScope] || "Transcript search";
  }

  function searchRoleLabel(record) {
    return {
      user: text(record.author_kind) === "owner_human" ? "owner prompt" : "prompt",
      assistant: "assistant",
      agent: "inter-agent",
      system: "system",
      external: "external",
      goal: "goal",
      tool: "tool",
      event: "event"
    }[text(record.role)] || text(record.role, "message");
  }

  function searchRecordAgent(record) {
    return app.agentsById.get(text(record.agent_id)) || null;
  }

  function searchRecordAgentLabel(record) {
    var agent = searchRecordAgent(record);
    if (agent) {
      return agentShortName(agent);
    }
    var path = text(record.agent_path, text(record.agent_id, "Unknown agent"));
    var pieces = path.split("/").filter(Boolean);
    return pieces.length ? pieces[pieces.length - 1] : path;
  }

  function renderMatchedExcerpt(excerpt) {
    var container = htmlElement("p", "search-result-excerpt");
    if (excerpt.leadingOmitted > 0) {
      container.appendChild(document.createTextNode("…"));
    }
    var cursor = 0;
    excerpt.ranges.forEach(function (range) {
      var left = clamp(number(range[0], cursor), cursor, excerpt.text.length);
      var right = clamp(number(range[1], left), left, excerpt.text.length);
      if (left > cursor) {
        container.appendChild(document.createTextNode(excerpt.text.slice(cursor, left)));
      }
      var marked = document.createElement("mark");
      marked.textContent = excerpt.text.slice(left, right);
      container.appendChild(marked);
      cursor = right;
    });
    if (cursor < excerpt.text.length) {
      container.appendChild(document.createTextNode(excerpt.text.slice(cursor)));
    }
    if (excerpt.trailingOmitted > 0) {
      container.appendChild(document.createTextNode("…"));
    }
    return container;
  }

  function searchResultCard(item) {
    var record = item.record;
    var reference = text(record.ref);
    var card = htmlElement(
      "article",
      "search-result" + (reference === app.activeSearchRef ? " is-active" : "")
    );
    card.setAttribute("role", "listitem");
    card.dataset.messageRef = reference;
    var main = htmlElement("button", "search-result-main");
    main.type = "button";
    main.dataset.testid = "search-result-main";
    var heading = htmlElement("div", "search-result-heading");
    heading.append(
      htmlElement("strong", "search-result-agent", searchRecordAgentLabel(record)),
      htmlElement("span", "search-result-role", searchRoleLabel(record))
    );
    var metadata = htmlElement("div", "search-result-meta");
    metadata.append(
      htmlElement("time", "", formatFullTime(number(record.at_ms, NaN))),
      htmlElement("span", "", text(record.team, "unknown team"))
    );
    main.append(heading, metadata, renderMatchedExcerpt(item.excerpt));
    main.addEventListener("click", function (event) {
      if (event.detail !== 1) {
        return;
      }
      jumpToSearchRecord(record);
    });
    main.addEventListener("dblclick", function (event) {
      event.preventDefault();
      jumpToSearchRecord(record);
      openSearchMessageModal(record).catch(showDetailLoadError);
    });
    var open = htmlElement("button", "button search-result-open", "Open");
    open.type = "button";
    open.addEventListener("click", function () {
      jumpToSearchRecord(record);
      openSearchMessageModal(record).catch(showDetailLoadError);
    });
    card.append(main, open);
    return card;
  }

  function renderTranscriptSearchResults() {
    var visible = transcriptSearchActive() && Boolean(app.query);
    dom.searchResults.hidden = !visible;
    if (!visible) {
      dom.searchResultsList.replaceChildren();
      dom.searchResultsStatus.textContent = "";
      dom.searchResultsCount.textContent = "";
      return;
    }
    dom.searchResultsTitle.textContent = searchScopeTitle();
    if (app.transcriptSearchState === "loading" ||
        app.transcriptSearchState === "unloaded") {
      dom.searchResultsStatus.textContent = "Searching the full transcript…";
      dom.searchResultsCount.textContent = "";
      dom.searchResultsList.replaceChildren();
      return;
    }
    if (app.transcriptSearchState === "error" ||
        app.transcriptSearchState === "unavailable") {
      dom.searchResultsStatus.textContent = app.transcriptSearchError ||
        "Transcript search is unavailable.";
      dom.searchResultsCount.textContent = "";
      dom.searchResultsList.replaceChildren();
      return;
    }
    var shown = app.transcriptSearchResults.length;
    dom.searchResultsCount.textContent = app.transcriptSearchTotal > shown
      ? "Showing " + formatCount(shown) + " of " + formatCount(app.transcriptSearchTotal)
      : formatCount(app.transcriptSearchTotal) +
        (app.transcriptSearchTotal === 1 ? " match" : " matches");
    dom.searchResultsStatus.textContent = app.transcriptSearchTotal > shown
      ? "Results are truncated; refine the search to see a smaller set."
      : (shown ? "" : "No transcript messages match this search.");
    dom.searchResultsList.replaceChildren.apply(
      dom.searchResultsList,
      app.transcriptSearchResults.map(searchResultCard)
    );
  }

  function phaseForSearchRecord(record) {
    var at = number(record.at_ms, NaN);
    if (!Number.isFinite(at)) {
      return null;
    }
    var candidates = (app.phasesByAgent.get(text(record.agent_id)) || []).filter(
      function (phase) {
        return number(phase.start_ms, Infinity) <= at &&
          number(phase.end_ms, -Infinity) > at;
      }
    );
    candidates.sort(function (left, right) {
      return (number(left.end_ms, 0) - number(left.start_ms, 0)) -
          (number(right.end_ms, 0) - number(right.start_ms, 0)) ||
        text(left.id).localeCompare(text(right.id));
    });
    return candidates[0] || null;
  }

  function scrollSearchAgentIntoView(agentId) {
    window.requestAnimationFrame(function () {
      var row = app.rowByAgent.get(agentId);
      if (!row) {
        return;
      }
      var maximum = Math.max(0, dom.scroll.scrollHeight - dom.scroll.clientHeight);
      var target = row.index * ROW_HEIGHT -
        Math.max(0, (dom.scroll.clientHeight - ROW_HEIGHT) / 2);
      dom.scroll.scrollTop = clamp(target, 0, maximum);
    });
  }

  function selectSearchRecordLocation(record) {
    var agentId = text(record.agent_id);
    var phase = phaseForSearchRecord(record);
    if (phase) {
      setSelection({
        kind: "phase",
        agent_id: agentId,
        phase_id: text(phase.id),
        start_ms: number(phase.start_ms, 0),
        end_ms: number(phase.end_ms, 0)
      });
    } else {
      setSelection({ kind: "agent", agent_id: agentId });
    }
    scrollSearchAgentIntoView(agentId);
  }

  function jumpToSearchRecord(record) {
    var reference = text(record.ref);
    var at = number(record.at_ms, NaN);
    var agentId = text(record.agent_id);
    if (!reference || !Number.isFinite(at) || !app.agentsById.has(agentId)) {
      return;
    }
    app.activeSearchRef = reference;
    renderTranscriptSearchResults();
    zoomToRange(at - SEARCH_JUMP_SPAN_MS / 2, at + SEARCH_JUMP_SPAN_MS / 2);
    selectSearchRecordLocation(record);
    if (!shardedMode()) {
      return;
    }
    var recordTeam = text(record.team);
    var exactShards = app.shardCatalog.filter(function (shard) {
      // Schema 3 shards a day per team, so the record's own team narrows twelve fetches to one.
      // Schema 2's day objects hold every team and carry no `team` field, which is why this is a
      // condition on the shard rather than an unconditional filter.
      if (schema3Enabled() && recordTeam && text(shard.team) !== recordTeam) {
        return false;
      }
      return number(shard.start_ms, Infinity) <= at &&
        number(shard.end_ms, -Infinity) > at;
    });
    if (!exactShards.length) {
      return;
    }
    requestDetailShards(exactShards).promise.then(function () {
      if (app.activeSearchRef === reference) {
        selectSearchRecordLocation(record);
      }
    }).catch(showDetailLoadError);
  }

  function searchShardAt(team, at) {
    return app.searchCatalog.find(function (shard) {
      return text(shard.team) === team &&
        number(shard.start_ms, Infinity) <= at &&
        number(shard.end_ms, -Infinity) > at;
    }) || null;
  }

  function ensureSearchContext(record, request) {
    if (!shardedMode()) {
      return Promise.resolve([]);
    }
    var team = text(record.team);
    var reference = text(record.ref);
    var promptReference = text(record.record_type) === "prompt"
      ? reference
      : text(record.prompt_ref);
    if (!team || !promptReference) {
      return Promise.resolve([]);
    }
    var teamShards = app.searchCatalog.filter(function (shard) {
      return text(shard.team) === team;
    });
    // Schema 3 publishes one relationship sidecar per *team*, so the question "is there linkage
    // for this team" is asked of that stream and not of every day's catalogue entry -- which
    // carries no `linkage` reference at all and would otherwise answer "no" and drag the team's
    // whole text corpus back in.
    var hasLinkage = app.searchCorpusMode === "schema3"
      ? app.searchLinksByTeam.has(team)
      : teamShards.length > 0 && teamShards.every(function (shard) {
        return shard.linkage && typeof shard.linkage === "object";
      });
    var needed = [];
    if (!hasLinkage) {
      needed = teamShards;
    } else {
      var timestamps = [number(record.at_ms, NaN)];
      var promptAt = text(record.record_type) === "prompt"
        ? number(record.at_ms, NaN)
        : number(record.prompt_at_ms, NaN);
      if (Number.isFinite(promptAt)) {
        timestamps.push(promptAt);
      }
      array(app.searchResponsesByPrompt.get(promptReference)).forEach(function (response) {
        var at = number(response && response.at_ms, NaN);
        if (Number.isFinite(at)) {
          timestamps.push(at);
        }
      });
      var seen = new Set();
      timestamps.forEach(function (at) {
        if (!Number.isFinite(at)) {
          return;
        }
        var shard = searchShardAt(team, at);
        if (!shard) {
          return;
        }
        var url = shardKey(shard, "transcript search shard");
        if (!seen.has(url)) {
          seen.add(url);
          needed.push(shard);
        }
      });
    }
    return loadSearchItemsBounded(
      needed,
      loadTranscriptSearchShard,
      null,
      function () { return request === app.detailRequest; }
    );
  }

  function linkedSearchContext(record) {
    var reference = text(record.ref);
    var promptReference = text(record.prompt_ref);
    if (text(record.record_type) === "prompt") {
      promptReference = reference;
    }
    if (!promptReference) {
      return [];
    }
    var prompt = app.searchRecordsByRef.get(promptReference) || null;
    if (!prompt) {
      return [];
    }
    var responses = app.searchRecords.filter(function (candidate) {
      var candidateReference = text(candidate.ref);
      var recordType = text(candidate.record_type);
      return candidateReference !== promptReference &&
        text(candidate.prompt_ref) === promptReference &&
        (recordType === "response" || recordType === "inter_agent_response");
    });
    var context = [prompt];
    responses.sort(function (left, right) {
      return number(left.at_ms, 0) - number(right.at_ms, 0) ||
        text(left.ref).localeCompare(text(right.ref));
    });
    context.push.apply(context, responses);
    return context;
  }

  function renderSearchMessage(container, record, relationship) {
    var card = htmlElement("article", "search-message-card");
    card.dataset.messageRef = text(record.ref);
    var heading = htmlElement("header", "search-message-heading");
    var headingLeft = htmlElement("div", "search-message-heading-copy");
    if (relationship) {
      headingLeft.appendChild(htmlElement("span", "search-message-relationship", relationship));
    }
    headingLeft.appendChild(htmlElement("strong", "search-message-role", searchRoleLabel(record)));
    heading.append(
      headingLeft,
      htmlElement("time", "entry-time", formatFullTime(number(record.at_ms, NaN)))
    );
    var metadata = htmlElement("div", "search-message-meta");
    metadata.append(
      htmlElement("span", "", text(record.team, "unknown team")),
      htmlElement("span", "", searchRecordAgentLabel(record))
    );
    var fidelity = text(record.content_fidelity);
    if (fidelity && fidelity !== "verbatim") {
      metadata.appendChild(htmlElement("span", "", fidelity));
    }
    var body = htmlElement("pre", "search-message-text", text(record.text));
    card.append(heading, metadata, body);
    container.appendChild(card);
  }

  function renderSearchMessageContext(container, record) {
    var context = linkedSearchContext(record);
    if (!context.length) {
      container.appendChild(
        htmlElement(
          "div",
          "empty-message",
          record.prompt_in_scope === false
            ? "The mechanically linked prompt is outside this exported time slice."
            : "No mechanically linked prompt was recorded."
        )
      );
      return;
    }
    container.appendChild(htmlElement(
      "p",
      "search-context-note",
      "These relationships come from provider turn and thread identifiers, not text inference."
    ));
    var promptReference = text(record.record_type) === "prompt"
      ? text(record.ref)
      : text(record.prompt_ref);
    context.forEach(function (candidate) {
      renderSearchMessage(
        container,
        candidate,
        text(candidate.ref) === promptReference ? "Prompt" : "Linked response"
      );
    });
  }

  function openSearchMessageModal(record) {
    var request = app.detailRequest + 1;
    app.detailRequest = request;
    return ensureSearchContext(record, request).then(function () {
      if (request !== app.detailRequest) {
        return;
      }
      openModalBase(
        "Transcript · " + formatFullTime(number(record.at_ms, NaN)),
        searchRecordAgentLabel(record) + " · " + searchRoleLabel(record),
        "",
        null
      );
      var tabs = [{
        label: "Message",
        render: function (container) {
          renderSearchMessage(container, record, "Exact message");
        }
      }];
      if (text(record.record_type) === "prompt" || text(record.prompt_ref)) {
        tabs.push({
          label: "Prompt & responses",
          render: function (container) {
            renderSearchMessageContext(container, record);
          }
        });
      }
      activateTabs(tabs, 0);
    }).catch(function (error) {
      if (request !== app.detailRequest) {
        return;
      }
      throw error;
    });
  }

  function loadPhaseIndex() {
    if (schema3Enabled()) {
      // Schema 3 has no phase-index object. The cards are the spine's `phase_card` kind -- the
      // same nine fields, one record per phase, no `states` array -- read by line range from every
      // team's spine the first time anything asks. One round of ranges rather than a
      // content-addressed object, and nothing at all for a session that never opens a lifetime.
      if (app.phaseIndexReady) {
        return Promise.resolve(app.phaseIndexByAgent);
      }
      return Promise.all(
        Array.from(app.spineByTeam.keys()).map(ensurePhaseCards)
      ).then(function () {
        app.phaseIndexReady = true;
        updateShardDiagnostics();
        return app.phaseIndexByAgent;
      });
    }
    if (!app.phaseIndexReference) {
      return Promise.resolve(null);
    }
    if (!app.phaseIndexPromise) {
      var url = immutableTimelineObjectUrl(
        app.phaseIndexReference,
        "timeline phase index"
      );
      var request = fetchContentAddressedJson(
        app.phaseIndexReference,
        "timeline phase index"
      ).then(function (raw) {
        if (!raw || typeof raw !== "object" || number(raw.schema_version, NaN) !== 2 ||
            text(raw.kind) !== "timeline-phase-index") {
          throw new Error("Unsupported timeline phase index: " + url);
        }
        validateSchema2ObjectSourceDigest(raw, "Timeline phase index");
        var byAgent = new Map();
        var ids = new Set();
        array(raw.phases).forEach(function (phase) {
          if (!phase || typeof phase !== "object") {
            throw new Error("Timeline phase-index entries must be objects.");
          }
          var id = text(phase.id);
          var agentId = text(phase.agent_id);
          if (!id || !agentId || ids.has(id) || Array.isArray(phase.states)) {
            throw new Error("Timeline phase index has an invalid or duplicate entry.");
          }
          ids.add(id);
          if (!byAgent.has(agentId)) {
            byAgent.set(agentId, []);
          }
          byAgent.get(agentId).push(phase);
        });
        byAgent.forEach(function (phases) {
          phases.sort(function (left, right) {
            return number(left.start_ms, 0) - number(right.start_ms, 0) ||
              text(left.id).localeCompare(text(right.id));
          });
        });
        app.phaseIndexByAgent = byAgent;
        return byAgent;
      });
      var cached = request.catch(function (error) {
        if (app.phaseIndexPromise === cached) {
          app.phaseIndexPromise = null;
        }
        throw error;
      });
      app.phaseIndexPromise = cached;
    }
    return app.phaseIndexPromise;
  }

  function populateTeamFilter() {
    var current = app.selectedTeam;
    var all = htmlElement("option", "", "All teams");
    all.value = "";
    var options = [all];
    app.data.teams.slice().sort(compareTeamsByActivity).forEach(function (team) {
      var slug = text(team.slug);
      if (!slug) {
        return;
      }
      var label = text(team.label, slug);
      var option = htmlElement("option", "", label);
      option.value = slug;
      options.push(option);
    });
    dom.teamFilter.replaceChildren.apply(dom.teamFilter, options);
    dom.teamFilter.value = current;
  }

  function populateSummaryFiles() {
    var files = app.data.summary_files.filter(function (file) {
      return selectedTeamAllows(file);
    });
    var children = [];
    if (!files.length) {
      children.push(htmlElement("p", "summary-files-empty", "No summary files in this dataset."));
    } else {
      files.forEach(function (file) {
        var button = htmlElement("button", "summary-file-button");
        button.type = "button";
        var team = text(file.team);
        var kindLabel = text(file.kind, "summary");
        if (!app.selectedTeam && team) {
          kindLabel = team + " · " + kindLabel;
        }
        var kind = htmlElement("span", "summary-file-kind", kindLabel);
        var label = htmlElement(
          "span",
          "summary-file-label",
          text(file.label, text(file.period, text(file.path, "Summary")))
        );
        button.append(kind, label);
        button.addEventListener("click", function () {
          dom.summaryMenu.open = false;
          openMarkdownModal({
            eyebrow: text(file.kind, "Summary") + (file.period ? " · " + file.period : ""),
            title: text(file.label, text(file.period, "Summary")),
            path: text(file.path)
          });
        });
        children.push(button);
      });
    }
    dom.summaryFiles.replaceChildren.apply(dom.summaryFiles, children);
  }

  function lowerSearchText(values) {
    return values
      .map(function (value) { return text(value); })
      .filter(function (value) { return value.length > 0; })
      .join(" ")
      .toLocaleLowerCase();
  }

  function compactSearchText(value) {
    return text(value)
      .replace(/[\u0009-\u000d\u001c-\u0020\u0085\u00a0\u1680\u2000-\u200a\u2028\u2029\u202f\u205f\u3000]+/g, " ")
      .replace(/^ +| +$/g, "");
  }

  function searchQueryParts(value) {
    var query = compactSearchText(value);
    var parts = [];
    var pattern = /"([^"\\]*(?:\\.[^"\\]*)*)"|([^ ]+)/g;
    var match;
    while ((match = pattern.exec(query)) !== null) {
      if (match[1] !== undefined) {
        var quoted = match[1].replace(/\\"/g, '"').replace(/\\\\/g, "\\");
        if (quoted) {
          parts.push({ value: quoted, quoted: true });
        }
      } else if (match[2]) {
        parts.push({ value: match[2], quoted: false });
      }
    }
    return parts;
  }

  function allSearchRanges(haystack, needle) {
    var ranges = [];
    var cursor = 0;
    if (!needle) {
      return ranges;
    }
    while (ranges.length < 64) {
      var position = haystack.indexOf(needle, cursor);
      if (position < 0) {
        break;
      }
      ranges.push([position, position + needle.length]);
      cursor = position + Math.max(1, needle.length);
    }
    return ranges;
  }

  function searchWordCharacter(value) {
    return Boolean(value) && /[\p{L}\p{N}_]/u.test(value);
  }

  function searchCharacterBefore(value, index) {
    if (index <= 0) {
      return "";
    }
    var start = index - 1;
    var low = value.charCodeAt(start);
    if (low >= 0xdc00 && low <= 0xdfff && start > 0) {
      var high = value.charCodeAt(start - 1);
      if (high >= 0xd800 && high <= 0xdbff) {
        start -= 1;
      }
    }
    return value.slice(start, index);
  }

  function searchCharacterAt(value, index) {
    if (index < 0 || index >= value.length) {
      return "";
    }
    var end = index + 1;
    var high = value.charCodeAt(index);
    if (high >= 0xd800 && high <= 0xdbff && end < value.length) {
      var low = value.charCodeAt(end);
      if (low >= 0xdc00 && low <= 0xdfff) {
        end += 1;
      }
    }
    return value.slice(index, end);
  }

  function wholeSearchRanges(haystack, needle) {
    return allSearchRanges(haystack, needle).filter(function (range) {
      return !searchWordCharacter(searchCharacterBefore(haystack, range[0])) &&
        !searchWordCharacter(searchCharacterAt(haystack, range[1]));
    });
  }

  function smartSearchMatch(value, query) {
    var compact = compactSearchText(value);
    var compactQuery = compactSearchText(query);
    if (!compact || !compactQuery) {
      return null;
    }
    var comparable = asciiLowerSearchText(compact);
    var parts = searchQueryParts(compactQuery);
    if (!parts.length) {
      return null;
    }
    var ranges = [];
    for (var index = 0; index < parts.length; index += 1) {
      var part = parts[index];
      var sought = asciiLowerSearchText(part.value);
      var candidate = !part.quoted && /^[\p{L}\p{N}_-]+$/u.test(part.value)
        ? wholeSearchRanges(comparable, sought)
        : allSearchRanges(comparable, sought);
      if (!candidate.length) {
        return null;
      }
      ranges.push.apply(ranges, candidate);
    }
    ranges.sort(function (left, right) {
      return left[0] - right[0] || left[1] - right[1];
    });
    ranges = ranges.filter(function (range, index) {
      return index === 0 || range[0] !== ranges[index - 1][0] ||
        range[1] !== ranges[index - 1][1];
    }).slice(0, 64);
    var first = Math.min.apply(null, ranges.map(function (range) { return range[0]; }));
    var last = Math.max.apply(null, ranges.map(function (range) { return range[1]; }));
    var span = Math.max(1, last - first);
    var exact = comparable === asciiLowerSearchText(compactQuery);
    return {
      compact: compact,
      ranges: ranges,
      score: (exact ? 100000 : 0) + Math.max(0, 20000 - span) +
        ranges.length * 100 - Math.min(first, 10000)
    };
  }

  function searchExcerpt(match) {
    var first = Math.min.apply(null, match.ranges.map(function (range) { return range[0]; }));
    var start = Math.max(0, first - 120);
    var end = Math.min(match.compact.length, start + SEARCH_EXCERPT_CHARACTERS);
    return {
      text: match.compact.slice(start, end),
      ranges: match.ranges.filter(function (range) {
        return range[1] > start && range[0] < end;
      }).map(function (range) {
        return [Math.max(0, range[0] - start), Math.min(end, range[1]) - start];
      }),
      fullCharacters: match.compact.length,
      leadingOmitted: start,
      trailingOmitted: match.compact.length - end,
      truncated: start > 0 || end < match.compact.length
    };
  }

  function transcriptSearchActive() {
    return app.searchScope !== "labels";
  }

  function selectedTeamAllows(item) {
    if (!app.selectedTeam) {
      return true;
    }
    var itemTeam = text(item && item.team);
    if (itemTeam) {
      return itemTeam === app.selectedTeam;
    }
    var teams = app.data ? array(app.data.teams) : [];
    return teams.length === 1 && text(teams[0] && teams[0].slug) === app.selectedTeam;
  }

  function agentOfficialLeaf(agent) {
    var explicit = text(agent.official_leaf);
    if (explicit) {
      return explicit;
    }
    var official = text(agent.official_name, text(agent.path));
    var parts = official.split("/").filter(Boolean);
    return parts.length ? parts[parts.length - 1] : official;
  }

  function agentShortName(agent) {
    return text(
      agent.short_name,
      text(
        agent.nickname,
        text(agent.label, text(agentOfficialLeaf(agent), text(agent.id, "Unknown agent")))
      )
    );
  }

  function agentOfficialName(agent) {
    return text(
      agent.official_name,
      text(agent.path, text(agent.official_leaf, text(agent.nickname, text(agent.id))))
    );
  }

  function namesEqual(left, right) {
    return Boolean(left) && Boolean(right) &&
      left.toLocaleLowerCase() === right.toLocaleLowerCase();
  }

  function agentSecondaryName(agent) {
    var shortName = agentShortName(agent);
    var officialName = agentOfficialName(agent);
    var nickname = text(agent.nickname);
    var parts = [];
    if (officialName && !namesEqual(officialName, shortName)) {
      parts.push(officialName);
    }
    if (nickname && !namesEqual(nickname, officialName)) {
      parts.push("coordinator: " + nickname);
    }
    return parts.join(" · ");
  }

  function agentAccessibleName(agent) {
    var parts = ["Short name: " + agentShortName(agent)];
    var officialName = agentOfficialName(agent);
    var nickname = text(agent.nickname);
    if (officialName) {
      parts.push("Official name: " + officialName);
    }
    if (nickname) {
      parts.push("Coordinator nickname: " + nickname);
    }
    if (!agentSummaryAvailable(agent)) {
      parts.push("Lifetime summary: Summary not generated");
    } else if (text(agent.lifetime_summary)) {
      parts.push("Lifetime summary: " + text(agent.lifetime_summary));
    }
    return parts.join(". ");
  }

  function agentLifetimeSummary(agent) {
    if (!agentSummaryAvailable(agent)) {
      return "Summary not generated for this agent lifetime.";
    }
    return text(
      agent.lifetime_summary,
      "No lifetime summary is available; regenerate this archive's summaries."
    );
  }

  function frontEllipsize(value, maximumCharacters) {
    var content = text(value);
    var maximum = Math.max(2, Math.floor(number(maximumCharacters, 2)));
    if (content.length <= maximum) {
      return content;
    }
    return "…" + content.slice(-(maximum - 1));
  }

  function truncateLabel(value, width, preserveTail) {
    var content = text(value);
    var characterWidth = preserveTail ? 5.25 : 6.1;
    var maximum = Math.max(0, Math.floor(width / characterWidth));
    if (content.length <= maximum) {
      return content;
    }
    if (maximum < 5) {
      return "";
    }
    if (preserveTail) {
      return "…" + content.slice(-(maximum - 1));
    }
    return content.slice(0, maximum - 1).trimEnd() + "…";
  }

  function fitAgentSecondaryName(agent, width) {
    var full = agentSecondaryName(agent);
    if (!full || full.length * 5.25 <= width) {
      return full;
    }
    var officialName = agentOfficialName(agent);
    if (officialName && !namesEqual(officialName, agentShortName(agent))) {
      return truncateLabel(officialName, width, true);
    }
    return truncateLabel(full, width, true);
  }

  function agentTooltipIdentity(agent, includeLifetimeSummary) {
    var lines = [];
    var officialName = agentOfficialName(agent);
    var nickname = text(agent.nickname);
    if (officialName) {
      lines.push("Official: " + frontEllipsize(officialName, 64));
    }
    if (nickname && !namesEqual(nickname, officialName)) {
      lines.push("Coordinator nickname: " + nickname);
    }
    if (includeLifetimeSummary) {
      lines.push("", agentLifetimeSummary(agent));
    }
    return lines.join("\n");
  }

  function agentSearchText(agent) {
    return lowerSearchText([
      agent.id,
      agent.team,
      agent.path,
      agent.short_name,
      agent.official_name,
      agent.official_leaf,
      agent.label,
      agent.nickname,
      agentSummaryAvailable(agent) ? agent.lifetime_summary : "",
      agent.status
    ]);
  }

  function phaseSearchText(phase) {
    return lowerSearchText([
      phase.id,
      phaseSummaryAvailable(phase) ? phase.phrase : "",
      phaseSummaryAvailable(phase) ? phase.paragraph : ""
    ]);
  }

  function phaseDisplayPhrase(phase) {
    return phaseSummaryAvailable(phase)
      ? text(phase.phrase, "Agent phase")
      : "Activity window";
  }

  function phaseDisplayParagraph(phase) {
    return phaseSummaryAvailable(phase)
      ? text(phase.paragraph, "No paragraph summary available.")
      : "Summary not generated for this activity window. Raw transcript and statistics remain available where recorded.";
  }

  function edgeSearchText(edge) {
    var source = app.agentsById.get(text(edge.source_id));
    var target = app.agentsById.get(text(edge.target_id));
    return lowerSearchText([
      edge.id,
      edge.kind,
      edge.phrase,
      edge.paragraph,
      edge.full_text,
      source ? agentSearchText(source) : "",
      target ? agentSearchText(target) : ""
    ]);
  }

  function compareAgents(left, right) {
    var leftTeam = text(left.team);
    var rightTeam = text(right.team);
    if (leftTeam !== rightTeam) {
      var activityDifference = teamActivitySortScore(rightTeam) -
        teamActivitySortScore(leftTeam);
      if (activityDifference) {
        return activityDifference;
      }
    }
    var leftPath = agentOfficialName(left);
    var rightPath = agentOfficialName(right);
    if (leftPath && rightPath && leftPath !== rightPath) {
      return leftPath.localeCompare(rightPath, undefined, { numeric: true });
    }
    var startDifference = number(left.start_ms, 0) - number(right.start_ms, 0);
    if (startDifference) {
      return startDifference;
    }
    return agentShortName(left).localeCompare(agentShortName(right));
  }

  function buildRows() {
    var query = app.searchScope === "labels"
      ? app.query.trim().toLocaleLowerCase()
      : "";
    var transcriptQueryReady = transcriptSearchActive() && Boolean(app.query) &&
      app.transcriptSearchState === "ready";
    var eligible = app.data.agents.filter(function (agent) {
      return !app.selectedTeam || text(agent.team) === app.selectedTeam;
    });
    var eligibleById = new Map();
    eligible.forEach(function (agent) {
      eligibleById.set(text(agent.id), agent);
    });

    var directMatches = new Set();
    if (transcriptQueryReady) {
      app.transcriptMatchedAgentIds.forEach(function (id) {
        if (eligibleById.has(id)) {
          directMatches.add(id);
        }
      });
    } else if (query) {
      eligible.forEach(function (agent) {
        var id = text(agent.id);
        var phases = app.phasesByAgent.get(id) || [];
        var agentMatches = agentSearchText(agent).indexOf(query) >= 0;
        var phaseMatches = phases.some(function (phase) {
          return phaseSearchText(phase).indexOf(query) >= 0;
        });
        if (agentMatches || phaseMatches) {
          directMatches.add(id);
        }
      });
      app.data.edges.forEach(function (edge) {
        if (edgeSearchText(edge).indexOf(query) >= 0) {
          if (eligibleById.has(text(edge.source_id))) {
            directMatches.add(text(edge.source_id));
          }
          if (eligibleById.has(text(edge.target_id))) {
            directMatches.add(text(edge.target_id));
          }
        }
      });
    } else {
      eligible.forEach(function (agent) {
        directMatches.add(text(agent.id));
      });
    }

    var included = new Set(directMatches);
    directMatches.forEach(function (id) {
      var current = eligibleById.get(id);
      var guard = 0;
      while (current && current.parent_id && guard < eligible.length) {
        var parentId = text(current.parent_id);
        if (!eligibleById.has(parentId)) {
          break;
        }
        included.add(parentId);
        current = eligibleById.get(parentId);
        guard += 1;
      }
    });

    var children = new Map();
    var roots = [];
    eligible.forEach(function (agent) {
      var id = text(agent.id);
      if (!included.has(id)) {
        return;
      }
      var parentId = text(agent.parent_id);
      if (parentId && included.has(parentId)) {
        if (!children.has(parentId)) {
          children.set(parentId, []);
        }
        children.get(parentId).push(agent);
      } else {
        roots.push(agent);
      }
    });
    roots.sort(compareAgents);
    children.forEach(function (items) {
      items.sort(compareAgents);
    });

    var rows = [];
    var visited = new Set();
    function visit(agent, treeDepth) {
      var id = text(agent.id);
      if (!id || visited.has(id)) {
        return;
      }
      visited.add(id);
      rows.push({
        agent: agent,
        treeDepth: treeDepth,
        directMatch: directMatches.has(id),
        agentTextMatch: transcriptQueryReady
          ? directMatches.has(id)
          : (!query || agentSearchText(agent).indexOf(query) >= 0)
      });
      (children.get(id) || []).forEach(function (child) {
        visit(child, treeDepth + 1);
      });
    }
    roots.forEach(function (root) {
      visit(root, 0);
    });
    eligible
      .filter(function (agent) {
        return included.has(text(agent.id)) && !visited.has(text(agent.id));
      })
      .sort(compareAgents)
      .forEach(function (agent) {
        visit(agent, Math.max(0, number(agent.depth, 0)));
      });

    app.rowByAgent.clear();
    if (app.renderLod === "aggregate") {
      // Aggregate rendering needs the filtered agents only for team matching and
      // statistics. Packing thousands of invisible lifetimes dominated every
      // outer-zoom frame without affecting a single rendered object.
      app.rows = rows;
      app.laneCount = 0;
      return;
    }
    var packed = null;
    if (!app.perAgentTracks && timelineCore) {
      var lifetimeItems = rows.map(function (row, inputIndex) {
        var agent = row.agent;
        return {
          id: text(agent.id),
          start_ms: number(agent.start_ms, app.data.range.start_ms),
          end_ms: number(agent.end_ms, app.data.range.end_ms),
          official_name: agentOfficialName(agent),
          input_index: inputIndex,
          dedicated: !text(agent.parent_id)
        };
      });
      lifetimeItems = timelineCore.lifetimesWithin(
        lifetimeItems,
        app.viewStart,
        app.viewEnd
      );
      var visibleIds = new Set(lifetimeItems.map(function (item) {
        return text(item.id);
      }));
      rows = rows.filter(function (row) {
        return visibleIds.has(text(row.agent.id));
      });
      packed = timelineCore.packLifetimes(lifetimeItems);
    }
    app.rows = rows;
    rows.forEach(function (row, index) {
      var agentId = text(row.agent.id);
      row.index = packed ? number(packed.lane_by_id[agentId], index) : index;
      app.rowByAgent.set(text(row.agent.id), row);
    });
    app.laneCount = packed ? number(packed.lane_count, rows.length) : rows.length;
  }

  function measure() {
    var measured = Math.round(dom.axis.getBoundingClientRect().width || dom.card.clientWidth || 1000);
    app.width = Math.max(280, measured);
    var cssWidth = parseFloat(
      getComputedStyle(dom.card).getPropertyValue("--label-width")
    );
    app.labelWidth = Number.isFinite(cssWidth) ? cssWidth : 238;
    app.chartWidth = Math.max(1, app.width - app.labelWidth);
  }

  function configureTrackMode() {
    dom.card.classList.toggle("per-agent-mode", app.perAgentTracks);
    dom.card.classList.toggle("packed-mode", !app.perAgentTracks);
    if (app.perAgentTracks) {
      dom.card.style.removeProperty("--label-width");
    } else {
      dom.card.style.setProperty("--label-width", COMPACT_LABEL_WIDTH + "px");
    }
    dom.svg.setAttribute("data-track-mode", app.perAgentTracks ? "per-agent" : "packed");
  }

  function phaseTop() {
    return app.perAgentTracks ? PHASE_TOP : COMPACT_PHASE_TOP;
  }

  function phaseHeight() {
    return app.perAgentTracks ? PHASE_HEIGHT : COMPACT_PHASE_HEIGHT;
  }

  function timeToX(milliseconds) {
    var span = Math.max(1, app.viewEnd - app.viewStart);
    return app.labelWidth + ((milliseconds - app.viewStart) / span) * app.chartWidth;
  }

  function niceTickInterval(span, width) {
    var target = span / Math.max(2, width / 115);
    var intervals = [
      1000, 5000, 15000, 30000,
      60000, 5 * 60000, 15 * 60000, 30 * 60000,
      3600000, 3 * 3600000, 6 * 3600000, 12 * 3600000,
      24 * 3600000, 2 * 24 * 3600000, 7 * 24 * 3600000,
      14 * 24 * 3600000, 30 * 24 * 3600000, 90 * 24 * 3600000,
      180 * 24 * 3600000, 365 * 24 * 3600000
    ];
    for (var index = 0; index < intervals.length; index += 1) {
      if (intervals[index] >= target) {
        return intervals[index];
      }
    }
    return intervals[intervals.length - 1];
  }

  function computeTicks() {
    var span = app.viewEnd - app.viewStart;
    var interval = niceTickInterval(span, app.chartWidth);
    var first = Math.ceil(app.viewStart / interval) * interval;
    var ticks = [];
    for (var value = first; value <= app.viewEnd && ticks.length < 250; value += interval) {
      ticks.push(value);
    }
    app.axisTicks = ticks;
  }

  function renderAxis() {
    computeTicks();
    dom.axis.setAttribute("viewBox", "0 0 " + app.width + " 34");
    dom.axis.setAttribute("width", String(app.width));
    dom.axis.setAttribute("height", "34");
    var children = [];
    children.push(svgElement("rect", {
      x: 0, y: 0, width: app.labelWidth, height: 34, fill: "#111823"
    }));
    children.push(svgElement("line", {
      x1: app.labelWidth, y1: 0, x2: app.labelWidth, y2: 34, class: "label-divider"
    }));
    children.push(svgElement("text", {
      x: 14, y: 21, class: "axis-title"
    }, app.renderLod === "aggregate"
      ? "TEAM ACTIVITY"
      : (app.perAgentTracks ? "AGENT TRACKS" : "PACKED LANES")));
    var span = app.viewEnd - app.viewStart;
    app.axisTicks.forEach(function (tick) {
      var x = timeToX(tick);
      children.push(svgElement("line", {
        x1: x, y1: 21, x2: x, y2: 34, class: "axis-line"
      }));
      children.push(svgElement("text", {
        x: x + 4, y: 14, class: "axis-label"
      }, formatTick(tick, span)));
    });
    dom.axis.replaceChildren.apply(dom.axis, children);
    dom.viewRange.textContent =
      formatRange(app.viewStart, app.viewEnd) +
      " · " + app.timezone +
      " · " + formatDuration(span);
  }

  function formatDuration(milliseconds) {
    var seconds = Math.max(0, milliseconds / 1000);
    if (seconds < 90) {
      return Math.round(seconds) + " sec";
    }
    var minutes = seconds / 60;
    if (minutes < 90) {
      return Math.round(minutes) + " min";
    }
    var hours = minutes / 60;
    if (hours < 48) {
      return hours.toFixed(hours < 10 ? 1 : 0) + " hr";
    }
    var days = hours / 24;
    if (days < 120) {
      return days.toFixed(days < 10 ? 1 : 0) + " days";
    }
    return (days / 365.25).toFixed(1) + " years";
  }

  function phaseColor(agentId) {
    var hash = 0;
    for (var index = 0; index < agentId.length; index += 1) {
      hash = ((hash << 5) - hash + agentId.charCodeAt(index)) | 0;
    }
    return PHASE_COLORS[Math.abs(hash) % PHASE_COLORS.length];
  }

  function addDefs(svg) {
    var defs = svgElement("defs");
    var clip = svgElement("clipPath", { id: "chart-clip" });
    clip.appendChild(svgElement("rect", {
      x: app.labelWidth,
      y: 0,
      width: app.chartWidth,
      height: Math.max(1, app.laneCount * ROW_HEIGHT)
    }));
    defs.appendChild(clip);
    Object.keys(EDGE_COLORS).forEach(function (kind) {
      var marker = svgElement("marker", {
        id: "arrow-" + kind,
        markerWidth: 7,
        markerHeight: 7,
        refX: 6,
        refY: 3.5,
        orient: "auto",
        markerUnits: "strokeWidth"
      });
      marker.appendChild(svgElement("path", {
        d: "M0,0 L7,3.5 L0,7 Z",
        fill: EDGE_COLORS[kind]
      }));
      defs.appendChild(marker);
    });
    svg.appendChild(defs);
  }

  function edgeKind(value) {
    return normalizeKind(value, ["spawn", "continuation", "message", "result"], "other");
  }

  function stateKind(value) {
    return normalizeKind(value, ["active", "tool", "waiting", "idle", "blocked"], "idle");
  }

  function visibleRowBounds() {
    var top = Math.max(0, dom.scroll.scrollTop - ROW_HEIGHT * 2);
    var bottom = dom.scroll.scrollTop + dom.scroll.clientHeight + ROW_HEIGHT * 2;
    return {
      first: Math.max(0, Math.floor(top / ROW_HEIGHT)),
      last: Math.min(app.laneCount - 1, Math.ceil(bottom / ROW_HEIGHT)),
      top: top,
      bottom: bottom
    };
  }

  function edgePath(x1, y1, x2, y2, kind) {
    var structuralDirection =
      kind === "spawn" || kind === "continuation" ? -1 : kind === "result" ? 1 : 0;
    if (structuralDirection !== 0 && Math.abs(y2 - y1) > 2) {
      var structuralBend = 28;
      var verticalDirection = y2 > y1 ? 1 : -1;
      var approachY = y2 - verticalDirection * 9;
      var outerX = structuralDirection < 0
        ? Math.min(x1, x2) - structuralBend
        : Math.max(x1, x2) + structuralBend;
      return "M " + x1.toFixed(2) + " " + y1.toFixed(2) +
        " C " + outerX.toFixed(2) + " " + y1.toFixed(2) +
        ", " + outerX.toFixed(2) + " " + approachY.toFixed(2) +
        ", " + x2.toFixed(2) + " " + approachY.toFixed(2) +
        " L " + x2.toFixed(2) + " " + y2.toFixed(2);
    }
    var direction = x2 >= x1 ? 1 : -1;
    var bend = Math.max(20, Math.abs(x2 - x1) * 0.42);
    var control1 = x1 + direction * bend;
    var control2 = x2 - direction * bend;
    if (Math.abs(x2 - x1) < 32 && Math.abs(y2 - y1) > 2) {
      var verticalDirection = y2 > y1 ? 1 : -1;
      var hookDirection = x1 + 34 < app.width ? 1 : -1;
      var hookX = x1 + hookDirection * 28;
      var approachY = y2 - verticalDirection * 9;
      return "M " + x1.toFixed(2) + " " + y1.toFixed(2) +
        " C " + hookX.toFixed(2) + " " + y1.toFixed(2) +
        ", " + hookX.toFixed(2) + " " + approachY.toFixed(2) +
        ", " + x2.toFixed(2) + " " + approachY.toFixed(2) +
        " L " + x2.toFixed(2) + " " + y2.toFixed(2);
    }
    if (Math.abs(x2 - x1) < 32) {
      control1 = x1 + direction * 28;
      control2 = x2 + direction * 28;
    }
    return "M " + x1.toFixed(2) + " " + y1.toFixed(2) +
      " C " + control1.toFixed(2) + " " + y1.toFixed(2) +
      ", " + control2.toFixed(2) + " " + y2.toFixed(2) +
      ", " + x2.toFixed(2) + " " + y2.toFixed(2);
  }

  function edgeAgent(edge, side) {
    return app.agentsById.get(text(edge[side + "_id"]));
  }

  function edgeRouteShort(edge) {
    var source = edgeAgent(edge, "source");
    var target = edgeAgent(edge, "target");
    var sourceName = source ? agentShortName(source) : text(edge.source_id, "Unknown source");
    var targetName = target ? agentShortName(target) : text(edge.target_id, "Unknown target");
    return sourceName + " → " + targetName;
  }

  function edgeRouteDetail(edge) {
    var source = edgeAgent(edge, "source");
    var target = edgeAgent(edge, "target");
    var lines = ["Route: " + edgeRouteShort(edge)];
    if (source) {
      lines.push("From — " + agentAccessibleName(source));
    }
    if (target) {
      lines.push("To — " + agentAccessibleName(target));
    }
    return lines.join("\n");
  }

  function selectedAgentId() {
    if (!app.selection) {
      return "";
    }
    if (app.selection.kind === "agent" || app.selection.kind === "phase") {
      return text(app.selection.agent_id);
    }
    return "";
  }

  function agentsAreImmediateFamily(leftId, rightId) {
    var left = app.agentsById.get(leftId);
    var right = app.agentsById.get(rightId);
    return Boolean(
      left && right &&
      (text(left.parent_id) === rightId || text(right.parent_id) === leftId)
    );
  }

  function selectionClass(agentId, phase) {
    var selection = app.selection;
    if (!selection) {
      return "";
    }
    if (selection.kind === "rollup") {
      if (!phase) {
        return "";
      }
      return rangesOverlap(
        number(phase.start_ms, 0),
        number(phase.end_ms, 0),
        number(selection.start_ms, 0),
        number(selection.end_ms, 0)
      ) ? " is-selected" : " is-dimmed";
    }
    if (selection.kind === "edge") {
      if (agentId === text(selection.source_id) || agentId === text(selection.target_id)) {
        return " is-related";
      }
      return " is-dimmed";
    }
    var selectedId = selectedAgentId();
    if (agentId === selectedId) {
      if (selection.kind === "phase") {
        if (!phase) {
          return " is-related";
        }
        return text(phase.id) === text(selection.phase_id) ? " is-selected" : " is-dimmed";
      }
      return " is-selected";
    }
    if (agentsAreImmediateFamily(agentId, selectedId)) {
      return " is-related";
    }
    return " is-dimmed";
  }

  function searchMatchClass(agentId) {
    return transcriptSearchActive() && Boolean(app.query) &&
      app.transcriptMatchedAgentIds.has(agentId) ? " is-search-match" : "";
  }

  function setSelection(selection) {
    clearRangeSelectionState();
    app.selection = selection;
    hideContextMenu();
    hideLaneAgentMenu(false);
    scheduleRender();
  }

  function selectAgent(agent) {
    setSelection({ kind: "agent", agent_id: text(agent.id) });
  }

  function selectPhase(phase) {
    var agentId = text(phase.agent_id);
    var next = timelineCore
      ? timelineCore.nextPhaseSelection(
          app.selection,
          agentId,
          text(phase.id),
          number(phase.start_ms, 0),
          number(phase.end_ms, 0)
        )
      : { kind: "agent", agent_id: agentId };
    setSelection(next);
  }

  function hideContextMenu() {
    dom.contextMenu.hidden = true;
    dom.contextMenuActions.replaceChildren();
  }

  function hideLaneAgentMenu(restoreFocus) {
    var anchor = app.laneMenuAnchor;
    dom.laneMenu.hidden = true;
    dom.laneMenuActions.replaceChildren();
    app.laneMenuAnchor = null;
    if (anchor && anchor.isConnected) {
      anchor.setAttribute("aria-expanded", "false");
      if (restoreFocus) {
        anchor.focus();
      }
    }
  }

  function showLaneAgentMenu(event, laneIndex, anchor) {
    event.preventDefault();
    event.stopPropagation();
    hideTooltip();
    hideContextMenu();
    hideLaneAgentMenu(false);
    var laneAgents = app.rows
      .filter(function (row) { return row.index === laneIndex; })
      .map(function (row) { return row.agent; })
      .sort(function (left, right) {
        return number(left.start_ms, 0) - number(right.start_ms, 0) ||
          compareAgents(left, right);
      });
    if (!laneAgents.length) {
      return;
    }
    var laneName = laneIndex === 0 ? "Coordinator lane" : "Lane " + laneIndex;
    dom.laneMenuTitle.textContent = laneName + " · " + laneAgents.length +
      (laneAgents.length === 1 ? " agent" : " agents");
    var buttons = laneAgents.map(function (agent) {
      var button = htmlElement("button", "lane-agent-menu-action");
      button.type = "button";
      button.setAttribute("role", "menuitem");
      button.setAttribute("aria-label", "Select " + agentAccessibleName(agent));
      button.title = agentAccessibleName(agent);
      button.append(
        htmlElement("span", "lane-agent-menu-name", agentShortName(agent)),
        htmlElement(
          "span",
          "lane-agent-menu-official",
          truncateLabel(agentOfficialName(agent), 270, true)
        )
      );
      button.addEventListener("click", function () {
        hideLaneAgentMenu(false);
        selectAgent(agent);
      });
      return button;
    });
    dom.laneMenuActions.replaceChildren.apply(dom.laneMenuActions, buttons);
    app.laneMenuAnchor = anchor;
    anchor.setAttribute("aria-expanded", "true");
    dom.laneMenu.hidden = false;
    var anchorBox = anchor.getBoundingClientRect();
    var menuWidth = dom.laneMenu.offsetWidth;
    var menuHeight = dom.laneMenu.offsetHeight;
    var left = clamp(anchorBox.left, 8, window.innerWidth - menuWidth - 8);
    var top = anchorBox.bottom + 4;
    if (top + menuHeight > window.innerHeight - 8) {
      top = Math.max(8, anchorBox.top - menuHeight - 4);
    }
    dom.laneMenu.style.left = left + "px";
    dom.laneMenu.style.top = top + "px";
    if (buttons[0]) {
      buttons[0].focus();
    }
  }

  function showContextMenu(event, title, actions) {
    event.preventDefault();
    event.stopPropagation();
    hideTooltip();
    hideLaneAgentMenu(false);
    dom.contextMenuTitle.textContent = title;
    var buttons = actions.map(function (action) {
      var button = htmlElement("button", "context-menu-action", action.label);
      button.type = "button";
      button.setAttribute("role", "menuitem");
      button.addEventListener("click", function () {
        hideContextMenu();
        action.run();
      });
      return button;
    });
    dom.contextMenuActions.replaceChildren.apply(dom.contextMenuActions, buttons);
    dom.contextMenu.hidden = false;
    var menuWidth = Math.max(180, dom.contextMenu.offsetWidth);
    var menuHeight = dom.contextMenu.offsetHeight;
    dom.contextMenu.style.left = clamp(event.clientX, 8, window.innerWidth - menuWidth - 8) + "px";
    dom.contextMenu.style.top = clamp(event.clientY, 8, window.innerHeight - menuHeight - 8) + "px";
    if (buttons[0]) {
      buttons[0].focus();
    }
  }

  function zoomToRange(start, end) {
    var safeStart = number(start, app.viewStart);
    var safeEnd = Math.max(safeStart + 1, number(end, app.viewEnd));
    setView(safeStart, safeEnd);
  }

  function zoomToActivityRange(bounds, scope) {
    // Schema 3 keeps the zoom bounds out of the records and out of the first paint -- they are a
    // spine kind of their own, laid down last, 324,624 bytes across the archive. A page that
    // never zooms never reads them; a page that does reads one team's, once, and then this
    // function behaves exactly as it does under schema 2, because the record it consults is the
    // same record derived by the same function.
    if (schema3Enabled() && !hasField(bounds || {}, "activity_start_ms")) {
      var reference = activityBoundsRef(bounds, scope);
      ensureActivityBounds(boundsTeam(bounds)).then(function () {
        var recorded = app.activityBoundsByRef.get(reference);
        zoomWithActivityBounds(
          recorded ? Object.assign({}, bounds, recorded) : bounds,
          scope
        );
      }, function () {
        zoomWithActivityBounds(bounds, scope);
      });
      return;
    }
    zoomWithActivityBounds(bounds, scope);
  }

  function zoomWithActivityBounds(bounds, scope) {
    function finishZoom() {
      var activityRange = timelineCore.activityRangeWithin(
        bounds,
        app.data,
        MIN_VIEW_MS,
        scope
      );
      zoomToRange(
        activityRange ? activityRange.start_ms : bounds.start_ms,
        activityRange ? activityRange.end_ms : bounds.end_ms
      );
    }
    // The recorded-bounds fast path is chosen by whether the bounds are *there*, not by which
    // generation the page loaded. It used to require `schemaMode === "schema2"`, which was true
    // of the only generation that published them and became wrong the moment a second one did:
    // schema 3 publishes the same two numbers, derived by the same function, as a spine record
    // keyed by stable reference. A guard on the schema name would have made a correct record
    // unusable because of where it came from.
    var recordedStart = number(bounds && bounds.activity_start_ms, NaN);
    var recordedEnd = number(bounds && bounds.activity_end_ms, NaN);
    if (Number.isFinite(recordedStart) && Number.isFinite(recordedEnd) &&
        recordedEnd > recordedStart) {
      zoomToRange(recordedStart, recordedEnd);
      return;
    }
    // No recorded bounds: derive them from what is in memory. Under schema 1 that is the whole
    // timeline and the answer is immediate; under a sharded generation the events and edges of
    // the interval have to be fetched first, which is what the request below does.
    if (app.schemaMode === "schema1") {
      finishZoom();
      return;
    }
    var request = requestDetailShards(
      detailShardsForRange(
        number(bounds.start_ms, app.viewStart),
        number(bounds.end_ms, app.viewEnd),
        0
      )
    );
    request.promise.then(finishZoom).catch(function (error) {
      showDetailLoadError(error);
      finishZoom();
    });
  }

  function phaseContextMenu(event, phase, agent) {
    var agentScope = { agent_id: text(agent.id) };
    showContextMenu(event, text(phase.phrase, "Work phase"), [
      {
        label: "Zoom to work phase",
        run: function () {
          zoomToActivityRange(phase, {
            agent_id: agentScope.agent_id,
            phase_id: text(phase.id)
          });
        }
      },
      {
        label: "Zoom to agent lifetime",
        run: function () { zoomToActivityRange(agent, agentScope); }
      }
    ]);
  }

  function agentContextMenu(event, agent) {
    showContextMenu(event, agentShortName(agent), [
      {
        label: "Zoom to agent lifetime",
        run: function () {
          zoomToActivityRange(agent, { agent_id: text(agent.id) });
        }
      }
    ]);
  }

  function renderEdge(edge, layer, bounds, bufferStart, bufferEnd) {
    var kind = edgeKind(edge.kind);
    if (app.renderLod === "aggregate" ||
        (app.renderLod === "lifetime" && kind !== "spawn" &&
         kind !== "continuation" && kind !== "result")) {
      return;
    }
    var sourceRow = app.rowByAgent.get(text(edge.source_id));
    var targetRow = app.rowByAgent.get(text(edge.target_id));
    if (!sourceRow || !targetRow) {
      return;
    }
    var sourceTime = number(edge.source_ms, NaN);
    var targetTime = number(edge.target_ms, NaN);
    if (!Number.isFinite(sourceTime) || !Number.isFinite(targetTime)) {
      return;
    }
    if (!rangesOverlap(
      Math.min(sourceTime, targetTime),
      Math.max(sourceTime, targetTime),
      bufferStart,
      bufferEnd
    )) {
      return;
    }
    var displayState = timelineCore
      ? timelineCore.edgeDisplayState(
          edge,
          app.selection,
          app.showGlobalMessages,
          app.showHighlightedMessages
        )
      : "normal";
    if (displayState === "hidden") {
      return;
    }
    var rowBuffer = 2;
    var bothEndpointsVisible =
      sourceRow.index >= bounds.first - rowBuffer &&
      sourceRow.index <= bounds.last + rowBuffer &&
      targetRow.index >= bounds.first - rowBuffer &&
      targetRow.index <= bounds.last + rowBuffer;
    if (displayState !== "highlighted" && !bothEndpointsVisible) {
      return;
    }
    var y1 = sourceRow.index * ROW_HEIGHT + ROW_HEIGHT / 2;
    var y2 = targetRow.index * ROW_HEIGHT + ROW_HEIGHT / 2;
    if (Math.max(y1, y2) < bounds.top || Math.min(y1, y2) > bounds.bottom) {
      return;
    }
    var x1 = timeToX(sourceTime);
    var x2 = timeToX(targetTime);
    // At lifetime density even one fork and join per agent becomes a thicket. Preserve
    // structural context for the highlighted family, but leave the unselected overview clean.
    if (app.renderLod === "lifetime" && displayState !== "highlighted") {
      return;
    }
    var pathData = edgePath(x1, y1, x2, y2, kind);
    var group = svgElement("g", {
      class: "edge-group edge-state-" + displayState,
      tabindex: "0",
      role: "button",
      "data-edge-id": text(edge.id),
      "data-edge-state": displayState,
      "aria-label": text(edge.phrase, kind + " interaction") + ". " + edgeRouteDetail(edge)
    });
    var visible = svgElement("path", {
      d: pathData,
      class: "edge-visible edge-" + kind,
      "marker-end": "url(#arrow-" + kind + ")"
    });
    var hit = svgElement("path", { d: pathData, class: "edge-hit" });
    group.append(visible, hit);
    group.addEventListener("pointerenter", function (event) {
      showTooltip(
        event,
        text(edge.phrase, kind + " interaction") + " · " + edgeRouteShort(edge),
        text(edge.paragraph, text(edge.content_status, "No paragraph summary available.")) +
          "\n\n" + edgeRouteDetail(edge),
        text(edge.kind, kind) + " interaction"
      );
    });
    group.addEventListener("pointermove", positionTooltip);
    group.addEventListener("pointerleave", hideTooltip);
    group.addEventListener("click", function (event) {
      if (Date.now() < app.suppressClickUntil || event.detail !== 1) {
        return;
      }
      event.stopPropagation();
      setSelection({
        kind: "edge",
        edge_id: text(edge.id),
        source_id: text(edge.source_id),
        target_id: text(edge.target_id)
      });
    });
    group.addEventListener("dblclick", function (event) {
      event.preventDefault();
      event.stopPropagation();
      openEdgeModal(edge);
    });
    group.addEventListener("keydown", function (event) {
      if (event.key === "Enter") {
        event.preventDefault();
        openEdgeModal(edge);
      } else if (event.key === " ") {
        event.preventDefault();
        setSelection({
          kind: "edge",
          edge_id: text(edge.id),
          source_id: text(edge.source_id),
          target_id: text(edge.target_id)
        });
      }
    });
    layer.appendChild(group);
  }

  function truncatePhrase(phrase, width) {
    var maxCharacters = Math.max(0, Math.floor((width - 14) / 6.2));
    if (maxCharacters < 4) {
      return "";
    }
    if (phrase.length <= maxCharacters) {
      return phrase;
    }
    return phrase.slice(0, Math.max(1, maxCharacters - 1)).trimEnd() + "…";
  }

  function phaseMatchesQuery(phase, row) {
    if (!app.query) {
      return true;
    }
    if (transcriptSearchActive()) {
      return row.directMatch;
    }
    return row.agentTextMatch ||
      phaseSearchText(phase).indexOf(app.query.toLocaleLowerCase()) >= 0;
  }

  function renderPhase(phase, row, layer, bufferStart, bufferEnd) {
    var start = number(phase.start_ms, NaN);
    var end = number(phase.end_ms, NaN);
    if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) {
      return;
    }
    if (!rangesOverlap(start, end, app.viewStart, app.viewEnd) ||
        !rangesOverlap(start, end, bufferStart, bufferEnd) ||
        !phaseMatchesQuery(phase, row)) {
      return;
    }
    var clippedStart = Math.max(start, app.viewStart);
    var clippedEnd = Math.min(end, app.viewEnd);
    var x = timeToX(clippedStart);
    var width = Math.max(2, timeToX(clippedEnd) - x);
    var phaseBlockHeight = phaseHeight();
    var y = row.index * ROW_HEIGHT + phaseTop();
    var agentId = text(phase.agent_id);
    var agent = row.agent;
    var summaryAvailable = phaseSummaryAvailable(phase);
    var displayPhrase = phaseDisplayPhrase(phase);
    var group = svgElement("g", {
      class: "phase-group" + (summaryAvailable ? "" : " summary-not-generated") +
        selectionClass(agentId, phase) + searchMatchClass(agentId),
      tabindex: "0",
      role: "button",
      "data-phase-id": text(phase.id),
      "data-agent-id": agentId,
      "data-start-ms": String(start),
      "data-end-ms": String(end),
      "aria-pressed": app.selection && app.selection.kind === "phase" &&
        text(app.selection.phase_id) === text(phase.id) ? "true" : "false",
      "aria-label": displayPhrase +
        (summaryAvailable ? "" : ". Summary not generated") +
        ". " + agentAccessibleName(agent)
    });
    group.appendChild(svgElement("rect", {
      x: x,
      y: y,
      width: width,
      height: phaseBlockHeight,
      rx: 4,
      class: "phase-block",
      fill: phaseColor(agentId)
    }));

    array(phase.states).forEach(function (state) {
      var stateStart = number(state.start_ms, start);
      var stateEnd = number(state.end_ms, end);
      if (!rangesOverlap(stateStart, stateEnd, clippedStart, clippedEnd)) {
        return;
      }
      var stripStart = Math.max(stateStart, clippedStart);
      var stripEnd = Math.min(stateEnd, clippedEnd);
      var stripX = timeToX(stripStart);
      var stripWidth = Math.max(1, timeToX(stripEnd) - stripX);
      var kind = stateKind(state.kind);
      group.appendChild(svgElement("rect", {
        x: stripX,
        y: y + phaseBlockHeight - STATE_HEIGHT,
        width: stripWidth,
        height: STATE_HEIGHT,
        class: "state-strip",
        fill: STATE_COLORS[kind]
      }));
    });

    var phrase = truncatePhrase(displayPhrase, width);
    if (phrase) {
      group.appendChild(svgElement("text", {
        x: x + 7,
        y: y + 19,
        class: "phase-label"
      }, phrase));
    }
    group.addEventListener("pointerenter", function (event) {
      showTooltip(
        event,
        displayPhrase + " · " + agentShortName(agent),
        phaseDisplayParagraph(phase) +
          "\n\n" + agentTooltipIdentity(agent, false),
        formatStatsInline(phase.stats)
      );
    });
    group.addEventListener("pointermove", positionTooltip);
    group.addEventListener("pointerleave", hideTooltip);
    group.addEventListener("click", function (event) {
      if (Date.now() < app.suppressClickUntil || event.detail !== 1) {
        return;
      }
      event.stopPropagation();
      selectPhase(phase);
    });
    group.addEventListener("dblclick", function (event) {
      event.preventDefault();
      event.stopPropagation();
      openPhaseModal(phase, row.agent);
    });
    group.addEventListener("contextmenu", function (event) {
      phaseContextMenu(event, phase, row.agent);
    });
    group.addEventListener("keydown", function (event) {
      if (event.key === "Enter") {
        event.preventDefault();
        openPhaseModal(phase, row.agent);
      } else if (event.key === " ") {
        event.preventDefault();
        selectPhase(phase);
      }
    });
    layer.appendChild(group);
  }

  function renderTrackLabel(row, layer) {
    var agent = row.agent;
    var y = row.index * ROW_HEIGHT;
    var depth = Math.max(0, row.treeDepth);
    var maximumIndent = Math.max(15, app.labelWidth - 72);
    var indent = Math.min(15 + depth * 17, maximumIndent);
    var color = phaseColor(text(agent.id));
    var shortName = agentShortName(agent);
    var textX = indent + 13;
    var textWidth = Math.max(0, app.labelWidth - textX - 8);
    var secondaryName = fitAgentSecondaryName(agent, textWidth);
    var labelGroup = svgElement("g", {
      class: "agent-label-group" +
        (agentSummaryAvailable(agent) ? "" : " summary-not-generated") +
        selectionClass(text(agent.id), null) + searchMatchClass(text(agent.id)),
      role: "button",
      tabindex: "0",
      "data-agent-id": text(agent.id),
      "aria-label": agentAccessibleName(agent) +
        ". Hierarchy depth: " + depth + ". Status: " + text(agent.status, "unknown")
    });
    labelGroup.appendChild(svgElement("title", null, agentAccessibleName(agent)));
    labelGroup.appendChild(svgElement("rect", {
      x: 0,
      y: y,
      width: app.labelWidth,
      height: ROW_HEIGHT,
      class: "agent-label-hit"
    }));

    if (depth > 0) {
      var branchX = indent - 10;
      labelGroup.appendChild(svgElement("path", {
        d: "M " + branchX + " " + y +
          " L " + branchX + " " + (y + ROW_HEIGHT / 2) +
          " L " + (indent - 2) + " " + (y + ROW_HEIGHT / 2),
        class: "hierarchy-line"
      }));
    }
    labelGroup.appendChild(svgElement("circle", {
      cx: indent + 2,
      cy: y + 20,
      r: depth === 0 ? 5 : 4,
      class: "agent-dot",
      fill: color
    }));
    labelGroup.appendChild(svgElement("text", {
      x: textX,
      y: y + 17,
      class: "track-label" + (depth === 0 ? " track-label-coordinator" : "")
    }, truncateLabel(shortName, textWidth, false)));

    if (secondaryName) {
      labelGroup.appendChild(svgElement("text", {
        x: textX,
        y: y + 31,
        class: "track-official"
      }, secondaryName));
    }

    var status = text(agent.status, "unknown");
    var team = text(agent.team);
    var meta = (depth === 0 ? "COORDINATOR" : "L" + depth + " SUBAGENT") + " · " + status;
    if (team) {
      var teamLabel = app.teamBySlug.has(team)
        ? text(app.teamBySlug.get(team).label, team)
        : team;
      meta += " · " + teamLabel;
    }
    labelGroup.appendChild(svgElement("text", {
      x: textX,
      y: y + 45,
      class: "track-meta"
    }, truncateLabel(meta, textWidth, false)));
    labelGroup.addEventListener("pointerenter", function (event) {
      showTooltip(
        event,
        shortName,
        agentTooltipIdentity(agent, true),
        (depth === 0 ? "Coordinator" : "Hierarchy depth " + depth) + " · " + status
      );
    });
    labelGroup.addEventListener("pointermove", positionTooltip);
    labelGroup.addEventListener("pointerleave", hideTooltip);
    labelGroup.addEventListener("click", function (event) {
      if (event.detail !== 1 || Date.now() < app.suppressClickUntil) {
        return;
      }
      event.stopPropagation();
      selectAgent(agent);
    });
    labelGroup.addEventListener("contextmenu", function (event) {
      agentContextMenu(event, agent);
    });
    labelGroup.addEventListener("keydown", function (event) {
      if (event.key === " ") {
        event.preventDefault();
        selectAgent(agent);
      }
    });
    layer.appendChild(labelGroup);
  }

  function renderLaneLabel(index, layer) {
    var y = index * ROW_HEIGHT;
    var count = app.rows.filter(function (row) { return row.index === index; }).length;
    var displayName = index === 0 ? "COORD" : "LANE " + index;
    var accessibleName = index === 0 ? "Coordinator lane" : "Lane " + index;
    var group = svgElement("g", {
      class: "lane-label-group",
      role: "button",
      tabindex: "0",
      "data-lane-index": String(index),
      "aria-haspopup": "menu",
      "aria-expanded": "false",
      "aria-label": accessibleName + ", " + count +
        (count === 1 ? " named agent" : " named agents")
    });
    group.appendChild(svgElement("rect", {
      x: 0,
      y: y,
      width: app.labelWidth,
      height: ROW_HEIGHT,
      class: "lane-label-hit"
    }));
    group.appendChild(svgElement("text", {
      x: 12,
      y: y + 28,
      class: "lane-label"
    }, displayName));
    group.addEventListener("click", function (event) {
      if (event.detail !== 1 || Date.now() < app.suppressClickUntil) {
        return;
      }
      showLaneAgentMenu(event, index, group);
    });
    group.addEventListener("keydown", function (event) {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        showLaneAgentMenu(event, index, group);
      }
    });
    layer.appendChild(group);
  }

  function renderAgentLifetime(row, layer) {
    var agent = row.agent;
    var start = number(agent.start_ms, app.data.range.start_ms);
    var end = number(agent.end_ms, app.data.range.end_ms);
    if (!rangesOverlap(start, end, app.viewStart, app.viewEnd)) {
      return;
    }
    var clippedStart = Math.max(start, app.viewStart);
    var clippedEnd = Math.min(end, app.viewEnd);
    var x = timeToX(clippedStart);
    var width = Math.max(2, timeToX(clippedEnd) - x);
    var y = row.index * ROW_HEIGHT;
    var summaryAvailable = agentSummaryAvailable(agent);
    var group = svgElement("g", {
      class: "agent-lifetime-group" +
        (summaryAvailable ? "" : " summary-not-generated") +
        selectionClass(text(agent.id), null) + searchMatchClass(text(agent.id)),
      role: "button",
      tabindex: "0",
      "data-agent-id": text(agent.id),
      "data-start-ms": String(start),
      "data-end-ms": String(end),
      "aria-label": "Agent lifetime: " + agentAccessibleName(agent) +
        (summaryAvailable ? "" : ". Summary not generated")
    });
    group.appendChild(svgElement("rect", {
      x: x,
      y: y + 2,
      width: width,
      height: ROW_HEIGHT - 5,
      rx: 3,
      class: "agent-lifetime-hit"
    }));
    group.appendChild(svgElement("rect", {
      x: x,
      y: y + ROW_HEIGHT / 2 - 2,
      width: width,
      height: 4,
      rx: 2,
      class: "lifetime-line"
    }));
    if (!app.perAgentTracks && width >= 22) {
      group.appendChild(svgElement("text", {
        x: x + 4,
        y: y + 12,
        class: "agent-inline-name"
      }, truncatePhrase(agentShortName(agent), width)));
    }
    group.addEventListener("pointerenter", function (event) {
      showTooltip(
        event,
        agentShortName(agent),
        agentTooltipIdentity(agent, true),
        "Agent lifetime · " + formatRange(start, end)
      );
    });
    group.addEventListener("pointermove", positionTooltip);
    group.addEventListener("pointerleave", hideTooltip);
    group.addEventListener("click", function (event) {
      if (event.detail !== 1 || Date.now() < app.suppressClickUntil) {
        return;
      }
      event.stopPropagation();
      selectAgent(agent);
    });
    group.addEventListener("dblclick", function (event) {
      event.preventDefault();
      event.stopPropagation();
      openAgentLifetimeModal(agent);
    });
    group.addEventListener("contextmenu", function (event) {
      agentContextMenu(event, agent);
    });
    group.addEventListener("keydown", function (event) {
      if (event.key === "Enter") {
        event.preventDefault();
        openAgentLifetimeModal(agent);
      } else if (event.key === " ") {
        event.preventDefault();
        selectAgent(agent);
      }
    });
    layer.appendChild(group);
  }

  function aggregateTeams() {
    var matchedTeamIds = new Set(app.rows.map(function (row) {
      return text(row.agent.team);
    }));
    var teams = app.data.teams.filter(function (team) {
      var slug = text(team.slug);
      return (!app.selectedTeam || slug === app.selectedTeam) &&
        (!app.query || matchedTeamIds.has(slug));
    });
    if (teams.length) {
      return teams.sort(compareTeamsByActivity);
    }
    if (app.query) {
      return [];
    }
    var seen = new Set();
    return app.data.activity_bins.reduce(function (result, bin) {
      var slug = text(bin.team);
      if (slug && !seen.has(slug) && (!app.selectedTeam || slug === app.selectedTeam)) {
        seen.add(slug);
        result.push({ slug: slug, label: slug });
      }
      return result;
    }, []).sort(compareTeamsByActivity);
  }

  function combinedActivityBins(resolution, teamIndex) {
    var grouped = new Map();
    app.data.activity_bins.forEach(function (bin) {
      var team = text(bin.team);
      var start = number(bin.start_ms, NaN);
      var end = number(bin.end_ms, NaN);
      if (!teamIndex.has(team) || text(bin.resolution) !== resolution ||
          !Number.isFinite(start) || !Number.isFinite(end) || end <= start) {
        return;
      }
      var key = [team, String(start), String(end)].join(":");
      if (!grouped.has(key)) {
        grouped.set(key, {
          team: team,
          resolution: resolution,
          start_ms: start,
          end_ms: end,
          coordinator: null,
          workers: null
        });
      }
      var combined = grouped.get(key);
      if (text(bin.role) === "coordinator") {
        combined.coordinator = bin;
      } else if (text(bin.role) === "workers") {
        combined.workers = bin;
      }
    });
    return Array.from(grouped.values()).sort(function (left, right) {
      return teamIndex.get(left.team) - teamIndex.get(right.team) ||
        left.start_ms - right.start_ms || left.end_ms - right.end_ms;
    });
  }

  function aggregateSelectionKey(bin) {
    return [
      "activity",
      text(bin.team),
      text(bin.resolution),
      String(number(bin.start_ms, 0))
    ].join(":");
  }

  // How tall one team block is, and whether that height is the whole truth.
  //
  // `agents` is the team's size during the bin: the coordinator, counted as the fraction of the
  // bin it has activity evidence for, plus the average number of workers present. Counting the
  // coordinator as an agent rather than as a fixed 10px pedestal is what makes the height a
  // straight multiple of a count -- with a pedestal, one worker drew 16px and three drew 28px,
  // which is 1.75x for 3x the team and is exactly the misreading a linear scale is for.
  //
  // Returned as a record rather than a number because the caller needs all three parts: the
  // height to draw, whether it was clamped (so it can draw the torn edge), and the count itself
  // (so the tooltip can state the number the picture could not).
  function activityBinHeight(coordinatorPresence, averageWorkers) {
    var agents = clamp(number(coordinatorPresence, 0), 0, 1) +
      Math.max(0, number(averageWorkers, 0));
    var wanted = agents * AGGREGATE_PIXELS_PER_AGENT;
    return {
      agents: agents,
      height: clamp(wanted, AGGREGATE_BIN_MIN_HEIGHT, AGGREGATE_BIN_MAX_HEIGHT),
      // Saturated only when the clamp costs a visible amount. A block whose true height is a
      // third of a pixel over the cap is not misleading anybody, and marking it would spend the
      // torn edge -- whose whole value is that it is rare -- on rounding.
      saturated: wanted - AGGREGATE_BIN_MAX_HEIGHT > 0.5,
      capacityAgents: AGGREGATE_BIN_MAX_HEIGHT / AGGREGATE_PIXELS_PER_AGENT
    };
  }

  // The torn top edge of a clamped block: a saw-tooth polyline, the printer's convention for "cut
  // off, continues past here". Drawn instead of, say, a brighter fill because it survives the
  // three things that already modulate a block's colour -- coverage opacity, the summary-available
  // hue, and the hover and selection filters -- none of which leave a spare colour to mean this.
  function saturationEdgePoints(x, y, width) {
    var tooth = 4;
    var rise = 2.5;
    var steps = Math.max(2, Math.round(width / tooth));
    var points = [];
    for (var step = 0; step <= steps; step += 1) {
      points.push(
        (x + (width * step) / steps).toFixed(2) + "," +
        (y + (step % 2 ? rise : 0)).toFixed(2)
      );
    }
    return points.join(" ");
  }

  function renderActivityBin(bin, team, rowIndex, layer) {
    var start = number(bin.start_ms, NaN);
    var end = number(bin.end_ms, NaN);
    if (!Number.isFinite(start) || !Number.isFinite(end) || end <= app.viewStart ||
        start >= app.viewEnd || end <= start) {
      return;
    }
    var clippedStart = Math.max(start, app.viewStart);
    var clippedEnd = Math.min(end, app.viewEnd);
    var x = timeToX(clippedStart);
    var width = Math.max(1, timeToX(clippedEnd) - x);
    var coordinator = bin.coordinator || {};
    var workers = bin.workers || {};
    var coordinatorCoverage = clamp(
      number(
        coordinator.activity_evidence_fraction,
        number(coordinator.activity_coverage_fraction, 0)
      ),
      0,
      1
    );
    var workerCoverage = clamp(
      number(
        workers.activity_evidence_fraction,
        number(workers.activity_coverage_fraction, 0)
      ),
      0,
      1
    );
    var coverage = Math.max(coordinatorCoverage, workerCoverage);
    var average = Math.max(0, number(
      workers.avg_present_concurrency,
      number(workers.avg_active_concurrency, 0)
    ));
    var peak = Math.max(0, number(
      workers.peak_present_concurrency,
      number(workers.peak_concurrency, 0)
    ));
    var rowTop = rowIndex * AGGREGATE_TEAM_HEIGHT;
    var block = activityBinHeight(coordinatorCoverage, average);
    var height = block.height;
    var y = rowTop + 8;
    var key = aggregateSelectionKey(bin);
    var hasSummary = summaryAvailableInRange(text(bin.team), start, end);
    var selected = app.selection && app.selection.kind === "rollup" &&
      text(app.selection.path) === key;
    var group = svgElement("g", {
      class: "activity-bin-group " +
        (hasSummary ? "summary-available" : "summary-unavailable") +
        (selected ? " is-selected" : "") +
        (block.saturated ? " is-saturated" : ""),
      role: "button",
      tabindex: "0",
      "data-team": text(bin.team),
      "data-activity-role": "combined",
      "data-activity-resolution": text(bin.resolution),
      "data-summary-available": hasSummary ? "true" : "false",
      "data-start-ms": String(start),
      "data-end-ms": String(end),
      "data-agents-present": block.agents.toFixed(2),
      "data-height-saturated": block.saturated ? "true" : "false",
      "aria-pressed": selected ? "true" : "false",
      "aria-label": text(team.label, text(team.slug)) +
        " combined activity. " + formatRange(start, end) +
        ". " + block.agents.toFixed(1) + " agents present on average" +
        (block.saturated
          ? ", drawn clamped at " + block.capacityAgents.toFixed(0)
          : "") +
        (hasSummary ? ". Summary available" : ". No summary generated")
    });
    group.appendChild(svgElement("rect", {
      x: x,
      y: y,
      width: width,
      height: height,
      rx: 2,
      class: "activity-bin-block activity-bin-combined",
      "fill-opacity": (0.16 + coverage * 0.84).toFixed(3)
    }));
    if (block.saturated) {
      group.appendChild(svgElement("polyline", {
        points: saturationEdgePoints(x, y, width),
        class: "activity-bin-overflow",
        "aria-hidden": "true"
      }));
    }
    group.addEventListener("pointerenter", function (event) {
      showTooltip(
        event,
        text(team.label, text(team.slug)) + " · Team activity",
        formatRange(start, end) +
          "\nAgents present on average: " + block.agents.toFixed(2) +
          (block.saturated
            ? " (block height clamped at " + block.capacityAgents.toFixed(0) + ")"
            : "") +
          "\nCoordinator activity evidence: " +
            Math.round(coordinatorCoverage * 100) + "%" +
          "\nEstimated average present workers: " + average.toFixed(2) +
          "\nEstimated peak present workers: " + formatCount(peak) +
          "\nAny activity evidence: " + Math.round(coverage * 100) + "%" +
          "\nWorker evidence events: " +
            formatCount(workers.activity_evidence_events) +
          "\nDistinct active workers: " + formatCount(workers.distinct_active_agents) +
          "\nSummary: " + (hasSummary ? "available" : "not generated"),
        text(bin.resolution, "aggregate") +
          " combined mechanical activity bin · inferred timing"
      );
    });
    group.addEventListener("pointermove", positionTooltip);
    group.addEventListener("pointerleave", hideTooltip);
    group.addEventListener("click", function (event) {
      if (event.detail !== 1 || Date.now() < app.suppressClickUntil) {
        return;
      }
      event.stopPropagation();
      setSelection({
        kind: "rollup",
        path: key,
        start_ms: start,
        end_ms: end
      });
    });
    group.addEventListener("dblclick", function (event) {
      event.preventDefault();
      event.stopPropagation();
      zoomToRange(start, end);
    });
    group.addEventListener("contextmenu", function (event) {
      showContextMenu(event, "Mechanical activity bin", [{
        label: "Zoom to activity bin",
        run: function () { zoomToRange(start, end); }
      }]);
    });
    group.addEventListener("keydown", function (event) {
      if (event.key === "Enter") {
        event.preventDefault();
        zoomToRange(start, end);
      } else if (event.key === " ") {
        event.preventDefault();
        setSelection({
          kind: "rollup",
          path: key,
          start_ms: start,
          end_ms: end
        });
      }
    });
    layer.appendChild(group);
  }

  function renderAggregateTracks() {
    var teams = aggregateTeams();
    var totalHeight = Math.max(
      dom.scroll.clientHeight,
      teams.length * AGGREGATE_TEAM_HEIGHT,
      1
    );
    var resolution = timelineCore && typeof timelineCore.aggregateResolution === "function"
      ? timelineCore.aggregateResolution(app.viewStart, app.viewEnd, app.chartWidth)
      : "daily";
    dom.svg.setAttribute("viewBox", "0 0 " + app.width + " " + totalHeight);
    dom.svg.setAttribute("width", String(app.width));
    dom.svg.setAttribute("height", String(totalHeight));
    dom.svg.setAttribute("data-lane-count", String(teams.length));
    dom.svg.setAttribute("data-track-mode", "aggregate");
    dom.svg.setAttribute("data-render-lod", "aggregate");
    dom.svg.setAttribute("data-aggregate-resolution", resolution);
    dom.svg.setAttribute("data-activity-bin-count", "0");
    dom.svg.setAttribute("data-summary-bin-count", "0");
    dom.svg.replaceChildren();
    dom.empty.hidden = teams.length > 0;
    if (!teams.length) {
      return;
    }

    var backgroundLayer = svgElement("g");
    var gridLayer = svgElement("g");
    var contentLayer = svgElement("g");
    var labelLayer = svgElement("g");
    var rangeLayer = svgElement("g", { "aria-hidden": "true" });
    teams.forEach(function (team, index) {
      var y = index * AGGREGATE_TEAM_HEIGHT;
      backgroundLayer.appendChild(svgElement("rect", {
        x: 0,
        y: y,
        width: app.width,
        height: AGGREGATE_TEAM_HEIGHT,
        class: "aggregate-team-row " +
          (index % 2 ? "track-row-odd" : "track-row-even")
      }));
      backgroundLayer.appendChild(svgElement("line", {
        x1: 0,
        y1: y + AGGREGATE_TEAM_HEIGHT,
        x2: app.width,
        y2: y + AGGREGATE_TEAM_HEIGHT,
        class: "track-divider"
      }));
      labelLayer.appendChild(svgElement("text", {
        x: 12,
        y: y + 29,
        class: "aggregate-team-label"
      }, truncateLabel(text(team.label, text(team.slug, "Team")), app.labelWidth - 20, false)));
      labelLayer.appendChild(svgElement("text", {
        x: 12,
        y: y + 45,
        class: "aggregate-team-resolution"
      }, resolution + " activity"));
    });
    app.axisTicks.forEach(function (tick) {
      var x = timeToX(tick);
      gridLayer.appendChild(svgElement("line", {
        x1: x,
        y1: 0,
        x2: x,
        y2: totalHeight,
        class: "grid-line"
      }));
    });
    var teamIndex = new Map();
    teams.forEach(function (team, index) {
      teamIndex.set(text(team.slug), index);
    });
    var renderedBins = 0;
    var summaryBins = 0;
    combinedActivityBins(resolution, teamIndex).forEach(function (bin) {
      var index = teamIndex.get(text(bin.team));
      if (index === undefined) {
        return;
      }
      var before = contentLayer.childElementCount;
      renderActivityBin(bin, teams[index], index, contentLayer);
      if (contentLayer.childElementCount > before) {
        renderedBins += 1;
        if (summaryAvailableInRange(text(bin.team), bin.start_ms, bin.end_ms)) {
          summaryBins += 1;
        }
      }
    });
    dom.svg.setAttribute("data-activity-bin-count", String(renderedBins));
    dom.svg.setAttribute("data-summary-bin-count", String(summaryBins));
    labelLayer.appendChild(svgElement("rect", {
      x: app.labelWidth - 1,
      y: 0,
      width: 1,
      height: totalHeight,
      fill: "#344057"
    }));
    renderRangeSelection(rangeLayer, totalHeight);
    dom.svg.append(backgroundLayer, gridLayer, contentLayer, rangeLayer, labelLayer);
  }

  function renderRangeSelection(layer, height) {
    if (!app.rangeSelection) {
      dom.svg.classList.remove("is-range-selecting");
      dom.svg.removeAttribute("data-range-selection-state");
      return;
    }
    dom.svg.classList.add("is-range-selecting");
    dom.svg.setAttribute("data-range-selection-state", "active");
    var anchor = clamp(
      number(app.rangeSelection.anchor_ms, app.viewStart),
      app.viewStart,
      app.viewEnd
    );
    var cursor = clamp(
      number(app.rangeSelection.cursor_ms, anchor),
      app.viewStart,
      app.viewEnd
    );
    var anchorX = timeToX(anchor);
    var cursorX = timeToX(cursor);
    var left = Math.min(anchorX, cursorX);
    var width = Math.abs(cursorX - anchorX);
    if (width > 0.5) {
      layer.appendChild(svgElement("rect", {
        x: left,
        y: 0,
        width: width,
        height: Math.max(1, height),
        class: "range-selection-window"
      }));
    }
    layer.appendChild(svgElement("line", {
      x1: anchorX,
      y1: 0,
      x2: anchorX,
      y2: Math.max(1, height),
      class: "range-selection-line range-selection-anchor"
    }));
    layer.appendChild(svgElement("line", {
      x1: cursorX,
      y1: 0,
      x2: cursorX,
      y2: Math.max(1, height),
      class: "range-selection-line range-selection-cursor"
    }));
  }

  function renderTracks() {
    if (app.renderLod === "aggregate") {
      renderAggregateTracks();
      return;
    }
    dom.svg.removeAttribute("data-aggregate-resolution");
    dom.svg.removeAttribute("data-activity-bin-count");
    dom.svg.removeAttribute("data-summary-bin-count");
    var totalHeight = Math.max(dom.scroll.clientHeight, app.laneCount * ROW_HEIGHT, 1);
    dom.svg.setAttribute("viewBox", "0 0 " + app.width + " " + totalHeight);
    dom.svg.setAttribute("width", String(app.width));
    dom.svg.setAttribute("height", String(totalHeight));
    dom.svg.setAttribute("data-lane-count", String(app.laneCount));
    dom.svg.setAttribute("data-render-lod", app.renderLod);
    dom.svg.replaceChildren();
    addDefs(dom.svg);
    dom.empty.hidden = app.rows.length > 0;

    if (!app.rows.length) {
      return;
    }

    var bounds = visibleRowBounds();
    var backgroundLayer = svgElement("g");
    var gridLayer = svgElement("g", { "clip-path": "url(#chart-clip)" });
    var contentLayer = svgElement("g", { "clip-path": "url(#chart-clip)" });
    var edgeLayer = svgElement("g");
    var lifetimeLayer = svgElement("g");
    var phaseLayer = svgElement("g");
    var labelLayer = svgElement("g");
    var rangeLayer = svgElement("g", {
      "clip-path": "url(#chart-clip)",
      "aria-hidden": "true"
    });
    contentLayer.append(edgeLayer, lifetimeLayer, phaseLayer);

    for (var index = bounds.first; index <= bounds.last; index += 1) {
      var y = index * ROW_HEIGHT;
      backgroundLayer.appendChild(svgElement("rect", {
        x: 0,
        y: y,
        width: app.width,
        height: ROW_HEIGHT,
        class: "track-row " + (index % 2 ? "track-row-odd" : "track-row-even")
      }));
      backgroundLayer.appendChild(svgElement("line", {
        x1: 0,
        y1: y + ROW_HEIGHT,
        x2: app.width,
        y2: y + ROW_HEIGHT,
        class: "track-divider"
      }));
    }
    app.axisTicks.forEach(function (tick) {
      var x = timeToX(tick);
      gridLayer.appendChild(svgElement("line", {
        x1: x,
        y1: bounds.top,
        x2: x,
        y2: bounds.bottom,
        class: "grid-line"
      }));
    });

    var span = app.viewEnd - app.viewStart;
    var bufferStart = app.viewStart - span * 0.08;
    var bufferEnd = app.viewEnd + span * 0.08;
    if (app.renderLod !== "aggregate") {
      app.data.edges.forEach(function (edge) {
        var labelMatch = app.searchScope === "labels" && app.query &&
          edgeSearchText(edge).indexOf(app.query.toLocaleLowerCase()) >= 0;
        if (!app.query || labelMatch ||
            app.rowByAgent.has(text(edge.source_id)) || app.rowByAgent.has(text(edge.target_id))) {
          renderEdge(edge, edgeLayer, bounds, bufferStart, bufferEnd);
        }
      });
    }

    var visibleRows = app.rows.filter(function (row) {
      return row.index >= bounds.first && row.index <= bounds.last;
    });
    visibleRows.forEach(function (row) {
      var agent = row.agent;
      renderAgentLifetime(row, lifetimeLayer);
      if (app.renderLod === "detail") {
        (app.phasesByAgent.get(text(agent.id)) || []).forEach(function (phase) {
          renderPhase(phase, row, phaseLayer, bufferStart, bufferEnd);
        });
      }
      if (app.perAgentTracks) {
        renderTrackLabel(row, labelLayer);
      }
    });
    if (!app.perAgentTracks) {
      for (var laneIndex = bounds.first; laneIndex <= bounds.last; laneIndex += 1) {
        renderLaneLabel(laneIndex, labelLayer);
      }
    }
    labelLayer.appendChild(svgElement("rect", {
      x: app.labelWidth - 1,
      y: bounds.top,
      width: 1,
      height: Math.max(1, bounds.bottom - bounds.top),
      fill: "#344057"
    }));
    renderRangeSelection(rangeLayer, totalHeight);
    dom.svg.append(backgroundLayer, gridLayer, contentLayer, rangeLayer, labelLayer);
  }

  function renderRollups() {
    var visibleRollups = app.data.rollups.filter(function (rollup) {
      return selectedTeamAllows(rollup);
    });
    if (!visibleRollups.length) {
      dom.rollupRow.hidden = true;
      dom.rollupTrack.replaceChildren();
      return;
    }
    dom.rollupRow.hidden = false;
    var span = app.viewEnd - app.viewStart;
    var children = [];
    var visibleTeams = Array.from(new Set(visibleRollups.map(function (rollup) {
      return text(rollup.team);
    }).filter(Boolean))).sort();
    app.data.rollups.forEach(function (rollup) {
      var rollupTeam = text(rollup.team);
      if (!selectedTeamAllows(rollup)) {
        return;
      }
      var start = number(rollup.start_ms, NaN);
      var end = number(rollup.end_ms, start);
      if (!Number.isFinite(start) || !Number.isFinite(end) ||
          !rangesOverlap(start, end, app.viewStart, app.viewEnd)) {
        return;
      }
      var left = ((Math.max(start, app.viewStart) - app.viewStart) / span) * app.chartWidth;
      var right = ((Math.min(end, app.viewEnd) - app.viewStart) / span) * app.chartWidth;
      var width = Math.max(4, right - left);
      var kind = normalizeKind(
        rollup.kind,
        ["hourly", "daily", "weekly", "monthly", "quarterly"],
        "daily"
      );
      var selected = app.selection && app.selection.kind === "rollup" &&
        text(app.selection.path) === text(rollup.path);
      var summaryAvailable = rollupSummaryAvailable(rollup);
      var button = htmlElement(
        "button",
        "rollup-marker rollup-" + kind +
          (summaryAvailable ? "" : " summary-not-generated") +
          (selected ? " is-selected" : "")
      );
      button.type = "button";
      button.style.left = left.toFixed(2) + "px";
      button.style.width = width.toFixed(2) + "px";
      if (!app.selectedTeam && visibleTeams.length > 1 && rollupTeam) {
        var teamIndex = visibleTeams.indexOf(rollupTeam);
        var markerHeight = Math.max(4, Math.floor(18 / visibleTeams.length));
        var baseTop = {
          hourly: 3,
          daily: 29,
          weekly: 55,
          monthly: 81,
          quarterly: 107
        }[kind];
        button.style.top = (baseTop + Math.max(0, teamIndex) * markerHeight) + "px";
        button.style.height = markerHeight + "px";
        button.style.lineHeight = Math.max(4, markerHeight - 2) + "px";
        button.style.padding = "0 2px";
      }
      var teamLabel = app.teamBySlug.has(rollupTeam)
        ? text(app.teamBySlug.get(rollupTeam).label, rollupTeam)
        : rollupTeam;
      var markerLabel = text(rollup.label, kind);
      button.textContent = width >= 36 && (!teamLabel || app.selectedTeam)
        ? markerLabel
        : "";
      button.setAttribute(
        "aria-label",
        (teamLabel ? teamLabel + " " : "") + kind + " summary: " +
          text(rollup.label, formatRange(start, end)) +
          (summaryAvailable ? "" : ". Summary not generated")
      );
      button.title = (teamLabel ? teamLabel + " · " : "") + markerLabel +
        (summaryAvailable ? "" : " · Summary not generated");
      button.dataset.startMs = String(start);
      button.dataset.endMs = String(end);
      button.dataset.rollupKind = kind;
      button.setAttribute("aria-pressed", selected ? "true" : "false");
      button.addEventListener("click", function (event) {
        if (event.detail !== 1) {
          return;
        }
        setSelection({
          kind: "rollup",
          path: text(rollup.path),
          start_ms: start,
          end_ms: end
        });
      });
      button.addEventListener("dblclick", function (event) {
        event.preventDefault();
        openRollupModal(rollup, kind, start, end);
      });
      button.addEventListener("contextmenu", function (event) {
        var rangeName = {
          hourly: "hour",
          daily: "day",
          weekly: "week",
          monthly: "month",
          quarterly: "quarter"
        }[kind] || "summary range";
        showContextMenu(event, text(rollup.label, kind + " summary"), [
          {
            label: "Zoom to " + rangeName,
            run: function () {
              zoomToActivityRange(rollup, null);
            }
          }
        ]);
      });
      children.push(button);
    });
    dom.rollupTrack.replaceChildren.apply(dom.rollupTrack, children);
  }

  function eventBelongsToVisibleAgent(event, visibleIds) {
    var agentId = text(event.agent_id);
    if (!agentId) {
      return !app.selectedTeam && !app.query && visibleIds.size > 0;
    }
    return visibleIds.has(agentId);
  }

  function detailCoverageComplete(start, end) {
    if (!shardedMode()) {
      return true;
    }
    return app.shardCatalog.every(function (shard) {
      var overlaps = number(shard.start_ms, Infinity) < end &&
        number(shard.end_ms, -Infinity) > start;
      return !overlaps || app.loadedShardUrls.has(shardKey(shard, "detail shard"));
    });
  }

  function fullRangeAggregateStats() {
    function usableEventStats(value) {
      if (!value || typeof value !== "object") {
        return null;
      }
      var fields = [
        "user_prompts",
        "agent_responses",
        "inter_agent_messages",
        "external_messages",
        "tool_calls"
      ];
      return fields.every(function (field) {
        return Object.prototype.hasOwnProperty.call(value, field) &&
          Number.isFinite(Number(value[field]));
      }) ? value : null;
    }
    if (app.query || app.viewStart > app.data.range.start_ms ||
        app.viewEnd < app.data.range.end_ms) {
      return null;
    }
    if (!app.selectedTeam) {
      return usableEventStats(app.data.stats);
    }
    var team = app.teamBySlug.get(app.selectedTeam);
    return usableEventStats(team && team.stats);
  }

  function countVisibleStats() {
    var visibleIds = new Set(app.rows.map(function (row) {
      return text(row.agent.id);
    }));
    var eventCountsAvailable = detailCoverageComplete(app.viewStart, app.viewEnd);
    var aggregateStats = eventCountsAvailable ? null : fullRangeAggregateStats();
    var hasAggregateStats = aggregateStats !== null;
    var result = {
      user_prompts: eventCountsAvailable ? 0 :
        (hasAggregateStats ? number(aggregateStats.user_prompts, 0) : null),
      agent_responses: eventCountsAvailable ? 0 :
        (hasAggregateStats ? number(aggregateStats.agent_responses, 0) : null),
      inter_agent_messages: eventCountsAvailable ? 0 :
        (hasAggregateStats ? number(aggregateStats.inter_agent_messages, 0) : null),
      external_messages: eventCountsAvailable ? 0 :
        (hasAggregateStats ? number(aggregateStats.external_messages, 0) : null),
      tool_calls: eventCountsAvailable ? 0 :
        (hasAggregateStats ? number(aggregateStats.tool_calls, 0) : null),
      active_agents: 0,
      event_counts_available: eventCountsAvailable || hasAggregateStats
    };
    app.rows.forEach(function (row) {
      var agent = row.agent;
      if (rangesOverlap(
        number(agent.start_ms, app.data.range.start_ms),
        number(agent.end_ms, app.data.range.end_ms),
        app.viewStart,
        app.viewEnd
      )) {
        result.active_agents += 1;
      }
    });

    var eventsInRange = app.data.events.filter(function (event) {
      var at = number(event.at_ms, NaN);
      return Number.isFinite(at) &&
        at >= app.viewStart &&
        at < app.viewEnd &&
        eventBelongsToVisibleAgent(event, visibleIds);
    });
    if (eventCountsAvailable) {
      eventsInRange.forEach(function (event) {
        var kind = text(event.kind).toLowerCase().replace(/[- ]/g, "_");
        if (kind === "user_prompt" || kind === "user_prompts" || kind === "prompt") {
          result.user_prompts += 1;
        } else if (
          kind === "agent_response" ||
          kind === "agent_responses" ||
          kind === "assistant_message" ||
          kind === "coordinator_response"
        ) {
          result.agent_responses += 1;
        } else if (
          kind === "inter_agent_message" ||
          kind === "inter_agent_messages" ||
          kind === "agent_message"
        ) {
          result.inter_agent_messages += 1;
        } else if (kind === "external_message" || kind === "external_messages") {
          result.external_messages += 1;
        } else if (kind === "tool_call" || kind === "tool_calls" || kind === "tool") {
          result.tool_calls += 1;
        }
      });
    }
    return result;
  }

  function statNode(value, label) {
    var node = htmlElement("div", "stat");
    node.append(
      htmlElement("strong", "stat-value", value === null ? "—" : formatCount(value)),
      htmlElement("span", "stat-label", label)
    );
    return node;
  }

  function renderStats() {
    var stats = countVisibleStats();
    dom.statsRange.textContent =
      formatRange(app.viewStart, app.viewEnd) +
      (stats.event_counts_available ? "" : " · event counts unavailable");
    var nodes = [
      statNode(stats.user_prompts, "User prompts"),
      statNode(stats.agent_responses, "Agent responses"),
      statNode(stats.inter_agent_messages, "Inter-agent msgs"),
    ];
    if (Number(stats.external_messages) > 0) {
      nodes.push(statNode(stats.external_messages, "External msgs"));
    }
    nodes.push(
      statNode(stats.tool_calls, "Tool calls"),
      statNode(stats.active_agents, "Active agents")
    );
    dom.statsValues.replaceChildren.apply(dom.statsValues, nodes);
  }

  function render() {
    app.renderQueued = false;
    if (!app.data) {
      return;
    }
    var renderStarted = window.performance && typeof window.performance.now === "function"
      ? window.performance.now()
      : Date.now();
    configureTrackMode();
    measure();
    app.renderLod = timelineCore && typeof timelineCore.semanticZoomLevel === "function"
      ? timelineCore.semanticZoomLevel(app.viewStart, app.viewEnd, app.chartWidth)
      : "detail";
    if (app.renderLod === "aggregate") {
      dom.card.style.setProperty("--label-width", AGGREGATE_LABEL_WIDTH + "px");
      measure();
      app.renderLod = timelineCore && typeof timelineCore.semanticZoomLevel === "function"
        ? timelineCore.semanticZoomLevel(app.viewStart, app.viewEnd, app.chartWidth)
        : "aggregate";
    }
    requestVisibleDetails();
    dom.card.classList.toggle("aggregate-mode", app.renderLod === "aggregate");
    buildRows();
    dom.card.dataset.viewStartMs = String(app.viewStart);
    dom.card.dataset.viewEndMs = String(app.viewEnd);
    dom.card.dataset.trackMode = app.renderLod === "aggregate"
      ? "aggregate"
      : (app.perAgentTracks ? "per-agent" : "packed");
    if (app.selection) {
      dom.card.dataset.selectionScope = text(app.selection.kind);
    } else {
      dom.card.dataset.selectionScope = "none";
    }
    if (app.selection && (app.selection.kind === "agent" || app.selection.kind === "phase")) {
      dom.card.dataset.selectedAgentId = text(app.selection.agent_id);
    } else {
      dom.card.removeAttribute("data-selected-agent-id");
    }
    if (app.selection && app.selection.kind === "phase") {
      dom.card.dataset.selectedPhaseId = text(app.selection.phase_id);
    } else {
      dom.card.removeAttribute("data-selected-phase-id");
    }
    renderAxis();
    renderRollups();
    renderTracks();
    renderStats();
    var renderFinished = window.performance && typeof window.performance.now === "function"
      ? window.performance.now()
      : Date.now();
    app.renderRevision += 1;
    dom.card.dataset.renderLod = app.renderLod;
    dom.card.dataset.renderRevision = String(app.renderRevision);
    dom.card.dataset.renderDurationMs = Math.max(0, renderFinished - renderStarted).toFixed(2);
  }

  function scheduleRender() {
    if (app.renderQueued) {
      return;
    }
    app.renderQueued = true;
    window.requestAnimationFrame(render);
  }

  function setView(start, end) {
    if (!app.data || !Number.isFinite(start) || !Number.isFinite(end)) {
      return;
    }
    var next = timelineCore.boundedViewRange(
      start,
      end,
      app.navigationRange,
      MIN_VIEW_MS
    );
    clearRangeSelectionState();
    app.viewStart = next.start_ms;
    app.viewEnd = next.end_ms;
    scheduleRender();
  }

  function fitTimeline() {
    if (!app.data) {
      return;
    }
    setView(app.data.range.start_ms, app.data.range.end_ms);
  }

  function panTimelineByPixels(pixels) {
    var span = app.viewEnd - app.viewStart;
    var shift = (pixels / Math.max(1, app.chartWidth)) * span;
    setView(app.viewStart + shift, app.viewEnd + shift);
  }

  function zoomAround(ratio, scale) {
    var oldSpan = app.viewEnd - app.viewStart;
    var fullSpan = app.navigationRange.end_ms - app.navigationRange.start_ms;
    var newSpan = clamp(
      oldSpan * scale,
      Math.min(MIN_VIEW_MS, fullSpan),
      fullSpan
    );
    var anchorRatio = clamp(number(ratio, 0.5), 0, 1);
    var anchor = app.viewStart + anchorRatio * oldSpan;
    var start = anchor - anchorRatio * newSpan;
    setView(start, start + newSpan);
  }

  function wheelDeltaPixels(event) {
    if (event.deltaMode === WheelEvent.DOM_DELTA_LINE) {
      return event.deltaY * 16;
    }
    if (event.deltaMode === WheelEvent.DOM_DELTA_PAGE) {
      return event.deltaY * Math.max(120, dom.scroll.clientHeight);
    }
    return event.deltaY;
  }

  function wheelZoom(event) {
    if (!app.data) {
      return;
    }
    var horizontalDelta = event.shiftKey && Math.abs(event.deltaX) < 1
      ? event.deltaY
      : event.deltaX;
    var horizontalGesture = Math.abs(horizontalDelta) > 1 &&
      (event.shiftKey || Math.abs(horizontalDelta) >= Math.abs(event.deltaY) * 0.55);
    if (!event.ctrlKey && !event.metaKey && horizontalGesture) {
      event.preventDefault();
      hideTooltip();
      hideContextMenu();
      panTimelineByPixels(horizontalDelta);
      return;
    }
    var needsVerticalScroll =
      dom.scroll.scrollHeight > dom.scroll.clientHeight + 1;
    if (event.currentTarget === dom.svg &&
        needsVerticalScroll &&
        !event.ctrlKey &&
        !event.metaKey) {
      return;
    }
    event.preventDefault();
    hideTooltip();
    hideContextMenu();
    var rect = event.currentTarget.getBoundingClientRect();
    var ratio;
    if (event.currentTarget === dom.rollupTrack) {
      ratio = clamp((event.clientX - rect.left) / Math.max(1, rect.width), 0, 1);
    } else {
      var x = clamp(event.clientX - rect.left, app.labelWidth, app.width);
      ratio = clamp((x - app.labelWidth) / Math.max(1, app.chartWidth), 0, 1);
    }
    // Wheel hardware reports pixels, lines, or whole pages. Normalize first and cap one
    // browser event to a modest step so a single coarse notch cannot jump several zoom levels.
    var delta = clamp(wheelDeltaPixels(event), -80, 80);
    zoomAround(ratio, Math.exp(delta * 0.0015));
  }

  function eventTimeOnChart(event) {
    var rect = dom.svg.getBoundingClientRect();
    var x = clamp(event.clientX - rect.left, app.labelWidth, app.width);
    var ratio = clamp((x - app.labelWidth) / Math.max(1, app.chartWidth), 0, 1);
    return app.viewStart + ratio * (app.viewEnd - app.viewStart);
  }

  function clearRangeSelectionState() {
    app.rangeSelection = null;
    dom.svg.classList.remove("is-range-selecting");
    dom.svg.removeAttribute("data-range-selection-state");
  }

  function cancelRangeSelection() {
    if (!app.rangeSelection) {
      return;
    }
    clearRangeSelectionState();
    scheduleRender();
  }

  function updateRangeSelection(event) {
    if (!app.rangeSelection) {
      return;
    }
    app.rangeSelection.cursor_ms = eventTimeOnChart(event);
    scheduleRender();
  }

  function handleEmptyTrackClick(event) {
    if (Date.now() < app.suppressClickUntil) {
      return;
    }
    var target = event.target;
    if (!(target instanceof Element) ||
        (!target.classList.contains("track-row") &&
         !target.classList.contains("aggregate-team-row"))) {
      return;
    }
    if (event.clientX < dom.svg.getBoundingClientRect().left + app.labelWidth) {
      return;
    }
    if (app.rangeSelection) {
      var start = Math.min(
        number(app.rangeSelection.anchor_ms, app.viewStart),
        eventTimeOnChart(event)
      );
      var end = Math.max(
        number(app.rangeSelection.anchor_ms, app.viewStart),
        eventTimeOnChart(event)
      );
      clearRangeSelectionState();
      zoomToRange(start, Math.max(start + MIN_VIEW_MS, end));
      return;
    }
    if (event.detail !== 1) {
      return;
    }
    if (app.selection) {
      setSelection(null);
    }
    var at = eventTimeOnChart(event);
    app.rangeSelection = { anchor_ms: at, cursor_ms: at };
    dom.svg.classList.add("is-range-selecting");
    dom.svg.setAttribute("data-range-selection-state", "active");
    scheduleRender();
  }

  function cancelRangeSelectionOnContextMenu(event) {
    if (!app.rangeSelection) {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    cancelRangeSelection();
  }

  function beginPan(event) {
    if (!app.data || event.button !== 0 || app.rangeSelection) {
      return;
    }
    app.drag = {
      pointerId: event.pointerId,
      startX: event.clientX,
      lastX: event.clientX,
      viewStart: app.viewStart,
      viewEnd: app.viewEnd,
      moved: false,
      captured: false
    };
  }

  function continuePan(event) {
    if (!app.drag || event.pointerId !== app.drag.pointerId) {
      return;
    }
    var difference = event.clientX - app.drag.startX;
    app.drag.lastX = event.clientX;
    if (!app.drag.moved && Math.abs(difference) <= 3) {
      return;
    }
    if (!app.drag.moved) {
      app.drag.moved = true;
      hideTooltip();
      if (!app.drag.captured) {
        dom.svg.setPointerCapture(event.pointerId);
        dom.svg.classList.add("is-panning");
        app.drag.captured = true;
      }
    }
    var span = app.drag.viewEnd - app.drag.viewStart;
    var shift = -(difference / Math.max(1, app.chartWidth)) * span;
    setView(app.drag.viewStart + shift, app.drag.viewEnd + shift);
  }

  function endPan(event) {
    if (!app.drag || event.pointerId !== app.drag.pointerId) {
      return;
    }
    if (app.drag.moved) {
      app.suppressClickUntil = Date.now() + 180;
    }
    if (app.drag.captured && dom.svg.hasPointerCapture(event.pointerId)) {
      dom.svg.releasePointerCapture(event.pointerId);
    }
    app.drag = null;
    dom.svg.classList.remove("is-panning");
  }

  function keyboardNavigate(event) {
    if (!app.data || event.altKey || event.ctrlKey || event.metaKey) {
      return;
    }
    var target = event.target;
    if (target instanceof HTMLInputElement ||
        target instanceof HTMLSelectElement ||
        target instanceof HTMLTextAreaElement) {
      return;
    }
    var span = app.viewEnd - app.viewStart;
    if (event.key === "+" || event.key === "=") {
      event.preventDefault();
      var zoomedSpan = span * 0.6;
      var midpoint = app.viewStart + span / 2;
      setView(midpoint - zoomedSpan / 2, midpoint + zoomedSpan / 2);
    } else if (event.key === "-" || event.key === "_") {
      event.preventDefault();
      var expandedSpan = span / 0.6;
      var center = app.viewStart + span / 2;
      setView(center - expandedSpan / 2, center + expandedSpan / 2);
    } else if (event.key === "ArrowLeft") {
      event.preventDefault();
      setView(app.viewStart - span * 0.12, app.viewEnd - span * 0.12);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      setView(app.viewStart + span * 0.12, app.viewEnd + span * 0.12);
    } else if (event.key === "Home") {
      event.preventDefault();
      fitTimeline();
    }
  }

  function keyboardScrollTracks(event) {
    var amount = 0;
    if (event.key === "ArrowUp") {
      amount = -ROW_HEIGHT;
    } else if (event.key === "ArrowDown") {
      amount = ROW_HEIGHT;
    } else if (event.key === "PageUp") {
      amount = -Math.max(ROW_HEIGHT, dom.scroll.clientHeight - ROW_HEIGHT);
    } else if (event.key === "PageDown") {
      amount = Math.max(ROW_HEIGHT, dom.scroll.clientHeight - ROW_HEIGHT);
    } else {
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    dom.scroll.scrollBy({ top: amount, behavior: "auto" });
  }

  function showTooltip(event, title, body, stats) {
    dom.tooltipTitle.textContent = title;
    dom.tooltipBody.textContent = body;
    dom.tooltipStats.textContent = stats;
    dom.tooltipStats.hidden = !stats;
    dom.tooltip.hidden = false;
    positionTooltip(event);
  }

  function positionTooltip(event) {
    if (dom.tooltip.hidden) {
      return;
    }
    var gap = 14;
    var left = event.clientX + gap;
    var top = event.clientY + gap;
    var width = dom.tooltip.offsetWidth;
    var height = dom.tooltip.offsetHeight;
    if (left + width > window.innerWidth - 10) {
      left = event.clientX - width - gap;
    }
    if (top + height > window.innerHeight - 10) {
      top = event.clientY - height - gap;
    }
    dom.tooltip.style.left = Math.max(8, left) + "px";
    dom.tooltip.style.top = Math.max(8, top) + "px";
  }

  function hideTooltip() {
    dom.tooltip.hidden = true;
  }

  function modalStats(stats) {
    var row = htmlElement("div", "modal-stats");
    var values = [
      ["user_prompts", "prompts"],
      ["agent_responses", "responses"],
      ["inter_agent_messages", "inter-agent messages"],
      ["external_messages", "external messages"],
      ["tool_calls", "tool calls"]
    ];
    values.forEach(function (entry) {
      if (entry[0] === "external_messages" && !(Number(stats && stats[entry[0]]) > 0)) {
        return;
      }
      row.appendChild(
        htmlElement(
          "span",
          "modal-stat",
          formatCount(stats && stats[entry[0]]) + " " + entry[1]
        )
      );
    });
    return row;
  }

  function setModalSummary(paragraph, stats) {
    var children = [];
    if (paragraph) {
      children.push(markdownElement(paragraph, "modal-paragraph", true));
    }
    if (stats && typeof stats === "object") {
      children.push(modalStats(stats));
    }
    dom.modalSummary.replaceChildren.apply(dom.modalSummary, children);
    dom.modalSummary.hidden = children.length === 0;
  }

  function agentIdentityNode(agent, prefix) {
    var container = htmlElement("div", "modal-agent-identity");
    var heading = htmlElement("div", "modal-agent-heading");
    if (prefix) {
      heading.appendChild(htmlElement("span", "modal-agent-prefix", prefix));
    }
    heading.appendChild(htmlElement("strong", "modal-agent-short", agentShortName(agent)));
    container.appendChild(heading);

    var officialName = agentOfficialName(agent);
    if (officialName) {
      container.appendChild(
        htmlElement("div", "modal-agent-official", "Official: " + officialName)
      );
    }
    var nickname = text(agent.nickname);
    if (nickname && !namesEqual(nickname, officialName)) {
      container.appendChild(
        htmlElement("div", "modal-agent-nickname", "Coordinator nickname: " + nickname)
      );
    }
    container.setAttribute("aria-label", agentAccessibleName(agent));
    return container;
  }

  function showModalAgentIdentity(agent) {
    dom.modalSummary.prepend(agentIdentityNode(agent, "AGENT"));
    dom.modalSummary.hidden = false;
  }

  function showModalEdgeRoute(edge) {
    var route = htmlElement("div", "modal-edge-route");
    var source = edgeAgent(edge, "source");
    var target = edgeAgent(edge, "target");
    if (source) {
      route.appendChild(agentIdentityNode(source, "FROM"));
    }
    route.appendChild(htmlElement("span", "modal-route-arrow", "→"));
    if (target) {
      route.appendChild(agentIdentityNode(target, "TO"));
    }
    if (!source && !target) {
      route.replaceChildren(htmlElement("div", "modal-agent-official", edgeRouteShort(edge)));
    }
    route.setAttribute("aria-label", edgeRouteDetail(edge));
    dom.modalSummary.prepend(route);
    dom.modalSummary.hidden = false;
  }

  function openModalBase(eyebrow, title, paragraph, stats) {
    hideTooltip();
    var activeElement = document.activeElement;
    app.modalRestoreFocus =
      activeElement && typeof activeElement.focus === "function" ? activeElement : null;
    dom.modalEyebrow.textContent = eyebrow;
    dom.modalEyebrow.removeAttribute("title");
    dom.modalTitle.textContent = title;
    setModalSummary(paragraph, stats);
    dom.modalTabs.replaceChildren();
    dom.modalTabs.hidden = true;
    dom.modalContent.replaceChildren();
    dom.modalBackdrop.hidden = false;
    document.body.classList.add("modal-open");
    dom.modalClose.focus();
  }

  function closeModal() {
    if (dom.modalBackdrop.hidden) {
      return;
    }
    app.detailRequest += 1;
    dom.modalBackdrop.hidden = true;
    document.body.classList.remove("modal-open");
    if (app.modalRestoreFocus && document.contains(app.modalRestoreFocus)) {
      app.modalRestoreFocus.focus();
    }
    app.modalRestoreFocus = null;
  }

  function showLoading(container, message) {
    container.replaceChildren(
      htmlElement("div", "loading-message", message || "Loading details…")
    );
  }

  function showContentError(container, error) {
    var message = error instanceof Error ? error.message : String(error);
    container.replaceChildren(
      htmlElement("div", "error-message", "Could not load this view: " + message)
    );
  }

  function activateTabs(tabs, initialIndex) {
    dom.modalTabs.hidden = false;
    var buttons = [];
    var selected = -1;

    function activate(index) {
      if (index === selected || !tabs[index]) {
        return;
      }
      selected = index;
      buttons.forEach(function (button, buttonIndex) {
        var active = buttonIndex === index;
        button.setAttribute("aria-selected", active ? "true" : "false");
        button.tabIndex = active ? 0 : -1;
      });
      var panel = htmlElement("div", "modal-tab-panel");
      panel.setAttribute("role", "tabpanel");
      panel.setAttribute("aria-labelledby", "modal-tab-" + index);
      dom.modalContent.replaceChildren(panel);
      try {
        var result = tabs[index].render(panel);
        if (result && typeof result.catch === "function") {
          result.catch(function (error) {
            if (panel.isConnected && selected === index) {
              showContentError(panel, error);
            }
          });
        }
      } catch (error) {
        showContentError(panel, error);
      }
    }

    tabs.forEach(function (tab, index) {
      var button = htmlElement("button", "modal-tab", tab.label);
      button.type = "button";
      button.id = "modal-tab-" + index;
      button.setAttribute("role", "tab");
      button.setAttribute("aria-selected", "false");
      button.addEventListener("click", function () {
        activate(index);
      });
      button.addEventListener("keydown", function (event) {
        if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") {
          return;
        }
        event.preventDefault();
        var direction = event.key === "ArrowRight" ? 1 : -1;
        var next = (index + direction + buttons.length) % buttons.length;
        buttons[next].focus();
        activate(next);
      });
      buttons.push(button);
    });
    dom.modalTabs.replaceChildren.apply(dom.modalTabs, buttons);
    activate(clamp(initialIndex || 0, 0, Math.max(0, tabs.length - 1)));
  }

  function candidateUrls(path, preferredBase) {
    if (!path) {
      throw new Error("No file path was recorded for this item.");
    }
    var bases = [
      window.location.href,
      preferredBase,
      new URL(DATA_URL, window.location.href).href
    ].filter(Boolean);
    var candidates = [];
    var seen = new Set();
    bases.forEach(function (base) {
      var candidate = new URL(path, base);
      var sameOrigin = candidate.origin === window.location.origin;
      var allowedProtocol =
        candidate.protocol === "http:" ||
        candidate.protocol === "https:";
      if (!sameOrigin || !allowedProtocol || candidate.username || candidate.password) {
        return;
      }
      if (!seen.has(candidate.href)) {
        seen.add(candidate.href);
        candidates.push(candidate);
      }
    });
    if (!candidates.length) {
      throw new Error("Refusing to load a path outside this timeline site.");
    }
    return candidates;
  }

  async function fetchPath(path, format, preferredBase) {
    var candidates = candidateUrls(path, preferredBase);
    var lastError = null;
    for (var index = 0; index < candidates.length; index += 1) {
      try {
        var response = await fetch(candidates[index].href, {
          credentials: "same-origin"
        });
        if (!response.ok) {
          throw new Error("HTTP " + response.status + " for " + candidates[index].pathname);
        }
        return {
          content: format === "json" ? await response.json() : await response.text(),
          url: candidates[index].href
        };
      } catch (error) {
        lastError = error;
      }
    }
    throw lastError || new Error("Unable to load " + path);
  }

  function linkForUrl(url, label) {
    var link = htmlElement("a", "button button-primary", label);
    try {
      link.href = candidateUrls(url)[0].href;
      link.target = "_blank";
      link.rel = "noopener";
    } catch (_error) {
      link.removeAttribute("href");
      link.setAttribute("aria-disabled", "true");
    }
    return link;
  }

  function uniqueArtifactIds(value) {
    var seen = new Set();
    var result = [];
    array(value).forEach(function (rawId) {
      var id = text(rawId);
      if (/^artifact-[a-z0-9-]+$/.test(id) && !seen.has(id)) {
        seen.add(id);
        result.push(id);
      }
    });
    return result;
  }

  function artifactAssociation(owner, fallback) {
    var source = owner && typeof owner === "object" ? owner : {};
    var backup = fallback && typeof fallback === "object" ? fallback : {};
    var allIds = uniqueArtifactIds(
      Array.isArray(source.artifact_ids) ? source.artifact_ids : backup.artifact_ids
    );
    var outputIds = uniqueArtifactIds(
      Array.isArray(source.output_artifact_ids)
        ? source.output_artifact_ids
        : backup.output_artifact_ids
    );
    var allSet = new Set(allIds);
    outputIds.forEach(function (id) {
      if (!allSet.has(id)) {
        allSet.add(id);
        allIds.push(id);
      }
    });
    var outputSet = new Set(outputIds);
    return {
      all: allIds,
      outputs: allIds.filter(function (id) { return outputSet.has(id); }),
      references: allIds.filter(function (id) { return !outputSet.has(id); })
    };
  }

  function safeArtifactTarget(value) {
    var raw = text(value).trim();
    if (!raw || raw.startsWith("//")) {
      return null;
    }
    try {
      var url = new URL(raw, window.location.href);
      if (url.username || url.password) {
        return null;
      }
      var sameOrigin = url.origin === window.location.origin;
      if ((!sameOrigin && url.protocol !== "https:") ||
          (sameOrigin && url.protocol !== "http:" && url.protocol !== "https:")) {
        return null;
      }
      return { href: url.href, external: !sameOrigin };
    } catch (_error) {
      return null;
    }
  }

  function artifactLink(url, label, className) {
    var target = safeArtifactTarget(url);
    if (!target) {
      return htmlElement("span", (className || "") + " artifact-link-disabled", label);
    }
    var link = htmlElement("a", className || "", label);
    link.href = target.href;
    link.dataset.linkScope = target.external ? "external" : "internal";
    if (target.external) {
      link.target = "_blank";
      link.rel = "noopener noreferrer";
    }
    return link;
  }

  function artifactKindLabel(value) {
    var labels = {
      build_artifact: "Build artifact",
      commit: "Commit",
      diff: "Diff",
      gist: "Gist",
      issue: "Issue",
      merge_request: "Merge request",
      paste: "Paste",
      pull_request: "Pull request",
      repository: "Repository",
      task: "Task",
      uploaded_file: "Uploaded file",
      url: "Link"
    };
    return labels[text(value)] || "Artifact";
  }

  function artifactRelationLabel(value, output) {
    var labels = {
      produced: "Produced",
      published: "Published",
      updated: "Updated",
      referenced: "Referenced"
    };
    return labels[text(value)] || (output ? "Output" : "Referenced");
  }

  function artifactEvidence(artifact, context) {
    var records = array(artifact && artifact.evidence);
    if (!context || typeof context !== "object") {
      return records;
    }
    var start = number(context.start_ms, -Infinity);
    var end = number(context.end_ms, Infinity);
    var agentId = text(context.agent_id);
    return records.filter(function (evidence) {
      var at = number(evidence && evidence.timestamp_ms, NaN);
      return Number.isFinite(at) && at >= start && at < end &&
        (!agentId || text(evidence.thread_id) === agentId);
    });
  }

  function artifactRelations(artifact, context, output) {
    var outputRelations = new Set(["produced", "published", "updated"]);
    var seen = new Set();
    var result = [];
    artifactEvidence(artifact, context).forEach(function (evidence) {
      var relation = text(evidence.relation);
      if ((output && outputRelations.has(relation)) ||
          (!output && relation === "referenced")) {
        if (!seen.has(relation)) {
          seen.add(relation);
          result.push(relation);
        }
      }
    });
    return result.length ? result : [output ? "output" : "referenced"];
  }

  function phaseForArtifactEvidence(evidence) {
    var at = number(evidence && evidence.timestamp_ms, NaN);
    var agentId = text(evidence && evidence.thread_id);
    if (!Number.isFinite(at) || !agentId) {
      return null;
    }
    var phases = app.phasesByAgent.get(agentId) || [];
    for (var index = 0; index < phases.length; index += 1) {
      var phase = phases[index];
      if (at >= number(phase.start_ms, Infinity) && at < number(phase.end_ms, -Infinity)) {
        return phase;
      }
    }
    return null;
  }

  function artifactEvidenceLinks(artifact, context) {
    var container = htmlElement("div", "artifact-evidence-links");
    var seen = new Set();
    artifactEvidence(artifact, context).forEach(function (evidence) {
      var phase = phaseForArtifactEvidence(evidence);
      if (!phase || seen.has(text(phase.id))) {
        return;
      }
      var agent = app.agentsById.get(text(phase.agent_id));
      if (!agent) {
        return;
      }
      seen.add(text(phase.id));
      var button = htmlElement(
        "button",
        "artifact-phase-link",
        agentShortName(agent) + " · " + text(phase.phrase, "Work phase")
      );
      button.type = "button";
      button.dataset.phaseId = text(phase.id);
      button.addEventListener("click", function () {
        openPhaseModal(phase, agent);
      });
      container.appendChild(button);
    });
    return container;
  }

  function artifactCard(artifact, output, context) {
    var id = text(artifact.artifact_id);
    var card = htmlElement("article", "artifact-card");
    card.dataset.artifactId = id;
    card.dataset.artifactRole = output ? "output" : "reference";

    var heading = htmlElement("div", "artifact-card-heading");
    heading.appendChild(htmlElement("span", "artifact-kind", artifactKindLabel(artifact.kind)));
    artifactRelations(artifact, context, output).forEach(function (relation) {
      var badge = htmlElement(
        "span",
        "artifact-relation artifact-relation-" + normalizeKind(
          relation,
          ["produced", "published", "updated", "referenced", "output"],
          output ? "output" : "referenced"
        ),
        artifactRelationLabel(relation, output)
      );
      heading.appendChild(badge);
    });
    card.appendChild(heading);

    var label = text(artifact.label, text(artifact.title, artifactKindLabel(artifact.kind)));
    card.appendChild(artifactLink(artifact.url, label, "artifact-primary-link"));
    var title = text(artifact.title);
    if (title && title !== label) {
      card.appendChild(htmlElement("div", "artifact-title", title));
    }

    var projectLabel = text(artifact.project_slug);
    if (projectLabel) {
      var project = htmlElement("div", "artifact-project");
      project.append(
        document.createTextNode("Project: "),
        artifactLink(artifact.project_url, projectLabel, "artifact-project-link")
      );
      card.appendChild(project);
    }

    var evidence = artifactEvidence(artifact, context);
    if (evidence.length) {
      var actions = [];
      var seenActions = new Set();
      evidence.forEach(function (record) {
        var action = text(record.action).replace(/_/g, " ");
        if (action && !seenActions.has(action)) {
          seenActions.add(action);
          actions.push(action);
        }
      });
      var firstAt = Math.min.apply(null, evidence.map(function (record) {
        return number(record.timestamp_ms, Infinity);
      }));
      var evidenceText = formatCount(evidence.length) +
        (evidence.length === 1 ? " evidence record" : " evidence records");
      if (Number.isFinite(firstAt)) {
        evidenceText += " · " + formatFullTime(firstAt);
      }
      if (actions.length) {
        evidenceText += " · " + actions.join(", ");
      }
      card.appendChild(htmlElement("div", "artifact-evidence", evidenceText));
      var phaseLinks = artifactEvidenceLinks(artifact, context);
      if (phaseLinks.childElementCount) {
        card.appendChild(phaseLinks);
      }
    }
    return card;
  }

  function artifactSection(title, role, ids, output, context) {
    var section = htmlElement("section", "artifact-section");
    section.dataset.artifactSection = role;
    section.appendChild(
      htmlElement("h3", "artifact-section-title", title + " (" + formatCount(ids.length) + ")")
    );
    var cards = [];
    ids.forEach(function (id) {
      var artifact = app.artifactsById.get(id);
      if (artifact) {
        cards.push(artifactCard(artifact, output, context));
      }
    });
    if (cards.length) {
      var grid = htmlElement("div", "artifact-grid");
      grid.replaceChildren.apply(grid, cards);
      section.appendChild(grid);
    } else {
      section.appendChild(htmlElement(
        "p",
        "artifact-section-empty",
        output
          ? "No output-changing artifact evidence was captured in this range."
          : "No separately referenced artifacts were captured in this range."
      ));
    }
    return section;
  }

  async function renderArtifacts(container, owner, fallback, context) {
    var association = artifactAssociation(owner, fallback);
    container.dataset.artifactCount = String(association.all.length);
    if (association.all.length &&
        (app.artifactCatalogState === "unloaded" ||
         app.artifactCatalogState === "loading")) {
      showLoading(container, "Loading artifact links…");
      await ensureArtifactCatalog(app.data);
      if (!container.isConnected) {
        return;
      }
      container.replaceChildren();
    }
    if (app.artifactCatalogState === "error") {
      container.appendChild(htmlElement(
        "div",
        "artifact-catalog-warning",
        "Artifact links are unavailable: " + app.artifactCatalogError
      ));
    }
    if (!association.all.length) {
      container.appendChild(htmlElement(
        "div",
        "empty-message",
        "No evidence-backed work artifacts were associated with this range."
      ));
      return;
    }
    container.append(
      artifactSection("Work outputs", "outputs", association.outputs, true, context),
      artifactSection(
        "Referenced artifacts",
        "references",
        association.references,
        false,
        context
      )
    );
    var knownCount = association.all.filter(function (id) {
      return app.artifactsById.has(id);
    }).length;
    if (knownCount < association.all.length && app.artifactCatalogState !== "error") {
      container.appendChild(htmlElement(
        "div",
        "artifact-catalog-warning",
        formatCount(association.all.length - knownCount) +
          " associated artifact record(s) were not present in this catalog."
      ));
    }
  }

  function artifactTab(owner, fallback, context) {
    var count = artifactAssociation(owner, fallback).all.length;
    if (!count) {
      return null;
    }
    return {
      label: "Artifacts (" + formatCount(count) + ")",
      render: function (container) {
        return renderArtifacts(container, owner, fallback, context);
      }
    };
  }

  function renderWorkSummary(container, entries) {
    var items = array(entries);
    if (!items.length) {
      container.appendChild(
        htmlElement("div", "empty-message", "No work-summary entries fall in this phase.")
      );
      return;
    }
    var list = htmlElement("ol", "work-summary-list");
    items.forEach(function (entry) {
      var item = htmlElement("li", "work-summary-item");
      item.append(
        htmlElement("time", "entry-time", formatFullTime(number(entry.at_ms, NaN))),
        array(entry.pull_requests).length
          ? referenceTextElement(
              text(entry.text, "No summary text."),
              entry.pull_requests,
              "entry-text"
            )
          : markdownElement(
              text(entry.text, "No summary text."),
              "entry-text",
              true
            )
      );
      list.appendChild(item);
    });
    container.appendChild(list);
  }

  function toolsLine(tools) {
    var items = array(tools)
      .map(function (tool) {
        var count = Math.max(1, Number(tool.count) || 1);
        return { name: text(tool.name, "tool"), count: count };
      })
      .filter(function (tool) { return tool.name; });
    if (!items.length) {
      return "";
    }
    var total = items.reduce(function (sum, tool) { return sum + tool.count; }, 0);
    var details = items.map(function (tool) {
      return formatCount(tool.count) + " " + tool.name;
    });
    return formatCount(total) + (total === 1 ? " tool used: " : " tools used: ") + details.join(", ");
  }

  function roleClass(role) {
    return "transcript-entry-" + transcriptRole(role);
  }

  function transcriptRole(role) {
    return normalizeKind(
      role,
      ["user", "assistant", "agent", "system", "tool"],
      "other"
    );
  }

  function renderTranscript(container, transcript, selectedRoles) {
    var entries = array(transcript);
    if (!entries.length) {
      container.appendChild(
        htmlElement("div", "empty-message", "No transcript entries fall in this phase.")
      );
      return;
    }
    var roles = [];
    entries.forEach(function (entry) {
      var role = transcriptRole(entry.role);
      if (roles.indexOf(role) < 0) {
        roles.push(role);
      }
    });
    var roleOrder = ["user", "assistant", "agent", "tool", "system", "other"];
    roles.sort(function (left, right) {
      return roleOrder.indexOf(left) - roleOrder.indexOf(right);
    });
    var activeRoles = selectedRoles || new Set(roles);
    if (!selectedRoles) {
      roles.forEach(function (role) { activeRoles.add(role); });
    }
    var controls = htmlElement("div", "transcript-filters");
    controls.setAttribute("aria-label", "Transcript message filters");
    controls.dataset.testid = "transcript-role-filters";
    var shortcuts = htmlElement("div", "transcript-filter-shortcuts");
    var choices = htmlElement("div", "transcript-filter-roles");
    var list = htmlElement("div", "transcript-list");
    function renderEntries() {
      var cards = [];
      entries.forEach(function (entry) {
        var normalizedRole = transcriptRole(entry.role);
        if (!activeRoles.has(normalizedRole)) {
          return;
        }
        var roleLabel = text(entry.role, "unknown");
        var card = htmlElement("article", "transcript-entry " + roleClass(roleLabel));
        card.dataset.role = normalizedRole;
        var header = htmlElement("header", "transcript-entry-head");
        header.append(
          htmlElement("span", "transcript-role", roleLabel),
          htmlElement("time", "entry-time", formatFullTime(number(entry.at_ms, NaN)))
        );
        card.appendChild(header);
        var condensed = toolsLine(entry.tools);
        if (condensed) {
          card.appendChild(htmlElement("div", "tool-condensation", condensed));
        } else {
          card.appendChild(
            referenceTextElement(text(entry.text, ""), entry.pull_requests, "entry-text")
          );
        }
        cards.push(card);
      });
      if (!cards.length) {
        cards.push(htmlElement("div", "empty-message", "No transcript messages match these filters."));
      }
      list.replaceChildren.apply(list, cards);
      choices.querySelectorAll("input[data-role]").forEach(function (checkbox) {
        checkbox.checked = activeRoles.has(checkbox.dataset.role);
      });
    }
    function shortcut(label, update) {
      var button = htmlElement("button", "transcript-filter-button", label);
      button.type = "button";
      button.addEventListener("click", function () {
        update();
        renderEntries();
      });
      shortcuts.appendChild(button);
    }
    shortcut("User only", function () {
      activeRoles.clear();
      activeRoles.add("user");
    });
    shortcut("Select all", function () {
      roles.forEach(function (role) { activeRoles.add(role); });
    });
    shortcut("Select none", function () { activeRoles.clear(); });
    roles.forEach(function (role) {
      var label = htmlElement("label", "transcript-filter-choice");
      var checkbox = document.createElement("input");
      checkbox.type = "checkbox";
      checkbox.dataset.role = role;
      checkbox.checked = activeRoles.has(role);
      checkbox.addEventListener("change", function () {
        if (checkbox.checked) {
          activeRoles.add(role);
        } else {
          activeRoles.delete(role);
        }
        renderEntries();
      });
      label.append(checkbox, document.createTextNode(role));
      choices.appendChild(label);
    });
    controls.append(shortcuts, choices);
    container.append(controls, list);
    renderEntries();
  }

  async function renderRawSummary(container, path, detailUrl) {
    showLoading(container, "Loading raw summary…");
    var loaded = await fetchPath(path, "text", detailUrl);
    var toolbar = htmlElement("div", "raw-summary-toolbar");
    toolbar.appendChild(linkForUrl(loaded.url, "Open Markdown file"));
    container.replaceChildren(
      toolbar,
      markdownElement(loaded.content, "markdown-document", false)
    );
  }

  function renderSummaryNotGenerated(container, scope) {
    var suffix = scope ? " for this " + scope : "";
    container.appendChild(htmlElement(
      "div",
      "summary-not-generated-notice",
      "Summary not generated" + suffix + ". Raw transcript, statistics, and artifacts remain available where recorded."
    ));
  }

  function phaseFallbackDetail(phase) {
    return {
      phrase: phaseDisplayPhrase(phase),
      paragraph: phaseSummaryAvailable(phase) ? text(phase.paragraph) : "",
      stats: phase.stats || {},
      work_summary: [],
      transcript: [],
      raw_summary_path: "",
      summary_available: phaseSummaryAvailable(phase)
    };
  }

  function renderAgentLifetimeOverview(container, agent, indexedPhases) {
    var phases = indexedPhases || app.phasesByAgent.get(text(agent.id)) || [];
    container.appendChild(
      htmlElement("h3", "agent-lifetime-section-title", "Work phases (" +
        formatCount(phases.length) + ")")
    );
    if (!phases.length) {
      container.appendChild(htmlElement(
        "div",
        "empty-message",
        "No work phases were recorded for this agent."
      ));
      return;
    }
    var list = htmlElement("div", "agent-lifetime-phase-list");
    phases.forEach(function (phase) {
      var summaryAvailable = phaseSummaryAvailable(phase);
      var button = htmlElement(
        "button",
        "agent-lifetime-phase" + (summaryAvailable ? "" : " summary-not-generated")
      );
      button.type = "button";
      button.dataset.phaseId = text(phase.id);
      button.append(
        htmlElement(
          "time",
          "agent-lifetime-phase-time",
          formatRange(number(phase.start_ms, 0), number(phase.end_ms, 0))
        ),
        htmlElement("strong", "agent-lifetime-phase-title", phaseDisplayPhrase(phase)),
        htmlElement("span", "agent-lifetime-phase-summary", phaseDisplayParagraph(phase))
      );
      button.addEventListener("click", function () {
        openPhaseModal(phase, agent);
      });
      list.appendChild(button);
    });
    container.appendChild(list);
  }

  async function renderAgentLifetimePhases(container, agent, request) {
    if (shardedMode()) {
      if (app.phaseIndexReference || schema3Enabled()) {
        if (!app.phaseIndexPromise) {
          showLoading(container, "Loading agent work phases…");
        }
        var index = await loadPhaseIndex();
        if (request !== app.detailRequest || dom.modalBackdrop.hidden ||
            !container.isConnected) {
          return;
        }
        container.replaceChildren();
        renderAgentLifetimeOverview(
          container,
          agent,
          (index && index.get(text(agent.id))) || []
        );
        return;
      }
      var start = number(agent.start_ms, app.data.range.start_ms);
      var end = number(agent.end_ms, app.data.range.end_ms);
      var shards = app.shardCatalog.filter(function (shard) {
        return number(shard.start_ms, Infinity) < end &&
          number(shard.end_ms, -Infinity) > start;
      });
      showLoading(container, "Loading agent work phases…");
      await requestDetailShards(shards).promise;
      if (request !== app.detailRequest || dom.modalBackdrop.hidden ||
          !container.isConnected) {
        return;
      }
      container.replaceChildren();
    }
    renderAgentLifetimeOverview(container, agent);
  }

  function openAgentLifetimeModal(agent) {
    var request = ++app.detailRequest;
    var start = number(agent.start_ms, app.data.range.start_ms);
    var end = number(agent.end_ms, app.data.range.end_ms);
    openModalBase(
      "Agent lifetime · " + formatRange(start, end),
      agentShortName(agent),
      agentLifetimeSummary(agent),
      null
    );
    dom.modalEyebrow.title = agentAccessibleName(agent);
    showModalAgentIdentity(agent);
    var tabs = [
      {
        label: "Work Phases",
        render: function (container) {
          return renderAgentLifetimePhases(container, agent, request);
        }
      }
    ];
    var artifacts = artifactTab(agent, null, {
      start_ms: start,
      end_ms: end,
      agent_id: agent.id
    });
    if (artifacts) {
      tabs.push(artifacts);
    }
    activateTabs(tabs, 0);
  }

  function showPhaseDetail(detail, phase, agent, detailUrl) {
    var summaryAvailable = phaseDetailSummaryAvailable(detail, phase);
    var phrase = summaryAvailable
      ? text(detail.phrase, text(phase.phrase, "Agent phase"))
      : "Activity window";
    var paragraph = summaryAvailable
      ? text(detail.paragraph, text(phase.paragraph))
      : "Summary not generated for this activity window.";
    var stats = detail.stats && typeof detail.stats === "object"
      ? detail.stats
      : (phase.stats || {});
    dom.modalTitle.textContent = phrase;
    dom.modalEyebrow.textContent =
      agentShortName(agent) +
      " · " + formatRange(number(phase.start_ms, 0), number(phase.end_ms, 0));
    dom.modalEyebrow.title = agentAccessibleName(agent);
    setModalSummary(paragraph, stats);
    showModalAgentIdentity(agent);
    var transcriptRoles = new Set(array(detail.transcript).map(function (entry) {
      return transcriptRole(entry.role);
    }));
    var tabs = [];
    if (summaryAvailable) {
      tabs.push({
        label: "Agent Work Summary",
        render: function (container) {
          renderWorkSummary(container, detail.work_summary);
        }
      });
    }
    tabs.push({
      label: "Full Transcript",
      render: function (container) {
        if (!summaryAvailable) {
          renderSummaryNotGenerated(container, "activity window");
        }
        renderTranscript(container, detail.transcript, transcriptRoles);
      }
    });
    if (summaryAvailable) {
      tabs.push({
        label: "Markdown Summary",
        render: function (container) {
          var path = text(detail.raw_summary_path);
          if (!path) {
            container.appendChild(
              htmlElement("div", "empty-message", "No raw summary file was recorded for this phase.")
            );
            return undefined;
          }
          return renderRawSummary(container, path, detailUrl);
        }
      });
    }
    var artifacts = artifactTab(detail, phase, {
      start_ms: phase.start_ms,
      end_ms: phase.end_ms,
      agent_id: phase.agent_id
    });
    if (artifacts) {
      tabs.splice(summaryAvailable ? tabs.length - 1 : tabs.length, 0, artifacts);
    }
    activateTabs(tabs, 0);
  }

  async function openPhaseModal(phase, agent) {
    var request = ++app.detailRequest;
    openModalBase(
      agentShortName(agent) + " · phase",
      phaseDisplayPhrase(phase),
      phaseDisplayParagraph(phase),
      phase.stats || {}
    );
    dom.modalEyebrow.title = agentAccessibleName(agent);
    showModalAgentIdentity(agent);
    showLoading(dom.modalContent, "Loading phase details…");
    var path = text(phase.detail_path);
    if (!path) {
      showPhaseDetail(phaseFallbackDetail(phase), phase, agent);
      return;
    }
    try {
      var loaded = await fetchPath(path, "json");
      if (request !== app.detailRequest || dom.modalBackdrop.hidden) {
        return;
      }
      var detail = loaded.content;
      if (!detail || typeof detail !== "object") {
        throw new Error("Phase detail is not a JSON object.");
      }
      showPhaseDetail(detail, phase, agent, loaded.url);
    } catch (error) {
      if (request !== app.detailRequest || dom.modalBackdrop.hidden) {
        return;
      }
      dom.modalTabs.hidden = true;
      showContentError(dom.modalContent, error);
    }
  }

  function openEdgeModal(edge) {
    app.detailRequest += 1;
    var kind = edgeKind(edge.kind);
    openModalBase(
      kind + " interaction · " +
        formatFullTime(number(edge.source_ms, number(edge.target_ms, NaN))),
      text(edge.phrase, kind + " interaction"),
      text(edge.paragraph),
      null
    );
    dom.modalEyebrow.title = edgeRouteDetail(edge);
    showModalEdgeRoute(edge);
    dom.modalTabs.hidden = true;
    var content = htmlElement("div", "edge-detail");
    var fullText = text(edge.full_text);
    if (fullText) {
      content.appendChild(htmlElement("pre", "edge-full-text", fullText));
    } else {
      content.appendChild(
        htmlElement(
          "div",
          "content-limitation",
          text(
            edge.content_status,
            "The full interaction text was not retained in this timeline dataset."
          )
        )
      );
    }
    dom.modalContent.replaceChildren(content);
  }

  async function openMarkdownModal(options) {
    var request = ++app.detailRequest;
    openModalBase(options.eyebrow, options.title, "", null);
    dom.modalTabs.hidden = true;
    showLoading(dom.modalContent, "Loading summary file…");
    try {
      var loaded = await fetchPath(options.path, "text");
      if (request !== app.detailRequest || dom.modalBackdrop.hidden) {
        return;
      }
      var toolbar = htmlElement("div", "raw-summary-toolbar");
      toolbar.appendChild(linkForUrl(loaded.url, "Open Markdown file"));
      dom.modalContent.replaceChildren(
        toolbar,
        markdownElement(loaded.content, "markdown-document", false)
      );
    } catch (error) {
      if (request === app.detailRequest && !dom.modalBackdrop.hidden) {
        showContentError(dom.modalContent, error);
      }
    }
  }

  function setGlossaryHash(id) {
    var suffix = id ? "/" + id : "";
    window.history.pushState(null, "", "#glossary" + suffix);
  }

  function openGlossaryEntry(entry, updateLocation) {
    var id = glossaryId(entry && entry.id);
    if (!id || app.glossaryById.get(id) !== entry) {
      return;
    }
    app.detailRequest += 1;
    if (updateLocation) {
      setGlossaryHash(id);
    }
    openModalBase(
      "Project glossary" + (text(entry.team) ? " · " + text(entry.team) : "") +
        " · " + text(entry.week, "term"),
      text(entry.term, "Glossary entry"),
      "",
      null
    );
    var metadata = htmlElement("div", "glossary-entry-meta");
    metadata.appendChild(
      htmlElement("span", "", formatCount(entry.occurrences) + " source occurrence(s)")
    );
    if (Number.isFinite(number(entry.introduced_at_ms, NaN))) {
      metadata.appendChild(
        htmlElement("span", "", "Introduced " + formatFullTime(number(entry.introduced_at_ms, 0)))
      );
    }
    var permalink = htmlElement("a", "glossary-permalink", "Permanent glossary link");
    permalink.href = "#glossary/" + id;
    metadata.appendChild(permalink);
    dom.modalContent.replaceChildren(
      metadata,
      markdownElement(
        "## Definition\n\n" +
          text(entry.definition, "No model-backed definition is available.") +
          "\n\n## First-use evidence\n\n> " +
          text(entry.context, "No source context was retained."),
        "glossary-entry",
        false
      )
    );
  }

  function openGlossaryCatalog(updateLocation) {
    if (updateLocation) {
      setGlossaryHash("");
    }
    var path = text(app.data && app.data.glossary_path);
    if (path) {
      openMarkdownModal({
        eyebrow: "Project terminology",
        title: "Project glossary",
        path: path
      });
      return;
    }
    openModalBase("Project terminology", "Project glossary", "", null);
    var list = htmlElement("div", "glossary-list");
    app.glossaryById.forEach(function (entry) {
      if (!selectedTeamAllows(entry)) {
        return;
      }
      var entryLabel = text(entry.term);
      if (!app.selectedTeam && text(entry.team)) {
        entryLabel += " · " + text(entry.team);
      }
      var link = htmlElement("a", "glossary-term-link", entryLabel);
      link.href = "#glossary/" + glossaryId(entry.id);
      link.addEventListener("click", function (event) {
        event.preventDefault();
        openGlossaryEntry(entry, true);
      });
      list.appendChild(link);
    });
    dom.modalContent.replaceChildren(list);
  }

  function openGlossaryFromHash() {
    if (!app.data || !window.location.hash.startsWith("#glossary")) {
      return;
    }
    var prefix = "#glossary/";
    if (!window.location.hash.startsWith(prefix)) {
      openGlossaryCatalog(false);
      return;
    }
    var encoded = window.location.hash.slice(prefix.length);
    var id;
    try {
      id = decodeURIComponent(encoded);
    } catch (_error) {
      return;
    }
    var entry = app.glossaryById.get(glossaryId(id));
    if (entry) {
      openGlossaryEntry(entry, false);
    }
  }

  function openRollupModal(rollup, kind, start, end) {
    app.detailRequest += 1;
    openModalBase(
      (text(rollup.team) ? text(rollup.team) + " · " : "") +
        kind + " rollup · " + formatRange(start, end),
      text(rollup.label, kind + " summary"),
      rollupSummaryAvailable(rollup) ? "" : "Summary not generated for this time range.",
      rollup.stats || {}
    );
    var technicalAvailable = rollupAudienceAvailable(
      rollup,
      "technical_summary_available"
    );
    var plainAvailable = rollupAudienceAvailable(
      rollup,
      "plain_language_summary_available"
    );
    var technicalPath = technicalAvailable
      ? text(rollup.technical_path, text(rollup.path))
      : "";
    var plainPath = plainAvailable ? text(rollup.plain_language_path) : "";
    var tabs = [];
    if (technicalPath) {
      tabs.push({
        label: "Technical",
        render: function (container) {
          return renderRawSummary(container, technicalPath, "");
        }
      });
    }
    if (plainPath) {
      tabs.push({
        label: "Plain Language",
        render: function (container) {
          return renderRawSummary(container, plainPath, "");
        }
      });
    }
    if (!tabs.length) {
      tabs.push({
        label: "Summary",
        render: function (container) {
          renderSummaryNotGenerated(container, kind + " time range");
        }
      });
    }
    var artifacts = artifactTab(rollup, null, {
      start_ms: start,
      end_ms: end
    });
    if (artifacts) {
      tabs.push(artifacts);
    }
    activateTabs(tabs, 0);
  }

  function showLoadError(error) {
    // Names the *newest* entry point rather than the oldest. `loadTimeline` tries schema 3, then
    // schema 2, then schema 1, and reaches here only when all three failed -- so quoting
    // `data/timeline.json`, which a current build does not even write, sent every reader looking
    // for a file whose absence was never the problem.
    dom.meta.textContent = "Timeline could not be loaded";
    dom.loadError.textContent =
      "Could not load " + SCHEMA_3_URL + " or the generations behind it: " +
      errorMessage(error) +
      ". Serve the generated site over HTTP so its data files are available.";
    dom.loadError.hidden = false;
  }

  function fetchJsonCached(url) {
    if (!app.resourcePromises.has(url)) {
      var request = (async function () {
        var response = await fetch(url, { credentials: "same-origin" });
        if (!response.ok) {
          var error = new Error("HTTP " + response.status + " " + response.statusText);
          error.httpStatus = response.status;
          throw error;
        }
        return response.json();
      }());
      var cached = request.catch(function (error) {
        if (app.resourcePromises.get(url) === cached) {
          app.resourcePromises.delete(url);
        }
        throw error;
      });
      app.resourcePromises.set(url, cached);
    }
    return app.resourcePromises.get(url);
  }

  function sha256Hex(bytes) {
    if (!window.crypto || !window.crypto.subtle) {
      return Promise.reject(
        new Error("This browser cannot verify timeline object SHA-256 digests.")
      );
    }
    return window.crypto.subtle.digest("SHA-256", bytes).then(function (digest) {
      return Array.from(new Uint8Array(digest)).map(function (byte) {
        return byte.toString(16).padStart(2, "0");
      }).join("");
    });
  }

  function fetchContentAddressedJson(reference, where) {
    var url = immutableTimelineObjectUrl(reference, where);
    var expectedBytes = number(reference.bytes, NaN);
    var checksByteCount = hasField(reference, "bytes");
    if (checksByteCount &&
        (!Number.isSafeInteger(expectedBytes) || expectedBytes < 0)) {
      return Promise.reject(new Error(where + ".bytes must be a non-negative integer."));
    }
    if (!app.resourcePromises.has(url)) {
      var expectedDigest = text(reference.sha256);
      var request = (async function () {
        var response = await fetch(url, { credentials: "same-origin" });
        if (!response.ok) {
          var error = new Error("HTTP " + response.status + " " + response.statusText);
          error.httpStatus = response.status;
          throw error;
        }
        var buffer = await response.arrayBuffer();
        if (checksByteCount && buffer.byteLength !== expectedBytes) {
          throw new Error(
            where + " byte count mismatch: expected " + expectedBytes +
            ", received " + buffer.byteLength + "."
          );
        }
        var actualDigest = await sha256Hex(buffer);
        if (actualDigest !== expectedDigest) {
          throw new Error(where + " SHA-256 mismatch.");
        }
        return JSON.parse(new TextDecoder("utf-8").decode(buffer));
      }());
      var cached = request.catch(function (error) {
        if (app.resourcePromises.get(url) === cached) {
          app.resourcePromises.delete(url);
        }
        throw error;
      });
      app.resourcePromises.set(url, cached);
    }
    return app.resourcePromises.get(url);
  }

  // =====================================================================================
  // Schema 3: one small bootstrap, and shards read a gzip member at a time over HTTP Range
  // =====================================================================================
  //
  // What this reads, and what it costs
  // ----------------------------------
  // A schema-2 first paint downloads `data/timeline-v2.json` and the content-addressed
  // `timeline-global` object it names: on the measured archive that is 3,289,110 + 1,213,271 =
  // 4,502,381 transferred bytes, which the browser then parses as 5,702,530 + 10,358,370 =
  // 16,060,900 bytes of JSON before it can draw anything. Most of the bootstrap is not structure
  // at all -- it is 2,059 pre-aggregated activity bins and, once a build has a transcript search
  // corpus, 4,527,592 base64 characters of Bloom filter, inlined into the one file that gates the
  // first frame.
  //
  // A schema-3 first paint downloads `data/timeline-v3.json` (89,298 bytes, or 168,703 with the
  // search streams) and then, in one parallel round, two published *line ranges* out of each
  // team's spine shard, plus the single activity-bins shard. Nothing else. Measured against the
  // same twelve-team archive, over the archive's own `serve.py`:
  //
  //     schema 2   4,502,381 bytes transferred    16,060,900 bytes of JSON parsed    2 requests
  //     schema 3   2,155,957 bytes transferred    16,135,089 bytes of JSON parsed   34 requests
  //
  // **2.09 times fewer bytes on the wire, and the same parse.** The parse figure is recorded
  // rather than rounded away because it does not improve: both generations have to materialise
  // roughly the same records to draw the same frame, and minifying them does not change how many
  // there are. What moves is transfer, and what moves it is that a schema-3 shard is stored
  // compressed with no plain twin and is served as it is stored.
  //
  // The 34 requests against 2 are the honest cost of seeking: one bootstrap, thirteen sidecars
  // totalling 8,218 bytes, and twenty member reads. On a loopback server that is free; on a
  // high-latency link it is twenty round trips that schema 2 did not make, which is the trade this
  // layout takes deliberately and the reason the reads are issued in parallel.
  //
  // Two spine kinds are *not* in that paint, and both are read later, by line range:
  //
  //     phase cards      340,835 bytes   on the first agent-lifetime open
  //     zoom bounds       78,228 bytes   on the first zoom
  //
  // Schema 2 defers the cards too, into a separate `timeline-phase-index` object, and that object
  // costs 826,578 bytes compressed and 9,632,001 parsed -- so the deferred half is 2.4 times
  // cheaper on the wire and 3.1 times cheaper to parse. Its zoom bounds are not deferred at all:
  // they are fields on records the first paint already had, which is why they cost schema 2
  // nothing extra here and are already counted in its 16,060,900.
  //
  // Where the seek happens, and why the browser does not decode multi-member gzip
  // ------------------------------------------------------------------------------
  // A schema-3 shard is a *concatenation* of independent gzip members, and its `.index.jsonl`
  // sidecar publishes, per member, the compressed byte range, the uncompressed byte range, the
  // line range, and the instant range. So "give me the records for this day" and "give me lines
  // [0, 682)" are both answered by: read the sidecar (a few hundred bytes of plain JSONL), pick
  // the members, and ask the server for exactly their bytes with a `Range` header.
  //
  // The server does the seeking. `serve.py` -- which is `standalone_server.py`, copied verbatim
  // into the archive -- answers a single byte range with a 206, honours `If-Range`, and answers
  // 416 for a range past the end. Each member is *itself* a complete, ordinary gzip stream, so
  // what comes back over the wire is never multi-member: it is one stream that
  // `DecompressionStream("gzip")` inflates natively. That split is the whole reason the format is
  // multi-member rather than one big deflate, and it is why nothing here ships a JavaScript
  // inflater.
  //
  // A server that ignores `Range` is still correct, and that is a property rather than an
  // accident: ignoring a range is a legal answer, `fetchShardMember` detects the whole-file reply
  // and slices the member out of it, and the archive therefore stays readable from a bucket, a
  // CDN or `python3 -m http.server`. It costs bandwidth and nothing else.
  //
  // **The rejected alternative was a dynamic endpoint in `serve.py`** -- `GET /-/v3/records?...`,
  // with the server reading the sidecar, inflating the members and handing back JSON, so the
  // browser would decode no gzip at all. It is refused for three reasons. The archive would stop
  // being a directory of static files: today `python3 -m http.server`, a bucket, a CDN or a
  // read-only mount all serve it, and every one of those would have started returning 404 for the
  // timeline. The sidecar's member table and its contiguity invariant would then have a second
  // implementation, in Python, in a file that is copied into archives and therefore frozen at
  // build time -- so a reader fix would need a rebuild of every archive rather than a page reload.
  // And it buys nothing measurable: the inflate is the same inflate, moved across the socket, and
  // the bytes on the wire go *up*, because JSON is what gzip was compressing.
  //
  // What is checked before a record is believed
  // -------------------------------------------
  // Schema 2 verifies a whole object against a SHA-256 in its name. A range read cannot do that
  // without defeating its own purpose, so the integrity story is assembled from parts that a
  // partial read can actually check, and all of them are checks against the *bootstrap*, which is
  // the file the page trusted first:
  //
  //   * the sidecar header's `c_size`, `u_size`, `c_sha256`, `u_sha256`, `record_count` and
  //     `member_count` must equal the catalogue entry's -- so a sidecar from another generation
  //     is rejected before a single byte of data is fetched;
  //   * the member table must be contiguous and must cover exactly `c_size`/`u_size`/
  //     `record_count`, the same invariant `seekable_jsonl.ChunkIndex._parse` enforces;
  //   * every 206 must report a total that equals `c_size`, so a shard that grew or shrank under
  //     the reader is caught on the first range rather than the last;
  //   * the ETag of the first response is remembered and sent as `If-Range` on every later one,
  //     so the server itself refuses to stitch two versions of a shard together; and
  //   * each inflated member must decode to exactly `u_len` bytes and exactly `n` lines. gzip's
  //     own CRC-32 and length trailer come free with the inflate.
  //
  // Together those say: these bytes are the bytes this bootstrap described. What they do not say
  // is that the bootstrap is the one the archive's builder wrote -- neither does schema 2, whose
  // digests are also read out of the file being validated.

  function schema3Enabled() {
    return app.schemaMode === "schema3";
  }

  //: Everything schema 3 installs, put back the way an older generation expects to find it.
  //:
  //: Called by both older loaders rather than only by the fallback path, because a *partial*
  //: schema-3 load is the case that matters: `loadSchema3` throws after it has already set
  //: `spineByTeam` or `phaseIndexReady`, and `loadSchema2` then runs against a page carrying half
  //: of a generation it does not read. The member and sidecar caches are keyed by immutable
  //: paths, so they are correct to keep -- but they are dropped anyway, because nothing is going
  //: to ask for them and a reader that cannot say why a cache is still there has a leak.
  function resetSchema3State() {
    app.phaseIndexReady = false;
    app.phaseCardPromises.clear();
    app.spineByTeam = new Map();
    app.searchBloomByTeam = new Map();
    app.searchLinksByTeam = new Map();
    app.shardIndexPromises.clear();
    app.memberPromises.clear();
    app.shardEtags.clear();
    app.activityBoundsByRef.clear();
    app.activityBoundsPromises.clear();
    app.searchBloomPromises.clear();
    app.searchLinkPromises.clear();
  }

  //: Whether the loaded generation answers detail from shards rather than holding everything in
  //: memory. Schema 2 and schema 3 both do; schema 1 does not. Written as a predicate because the
  //: guards that used to say `schemaMode !== "schema2"` were asking this question and not that
  //: one, and each of them became wrong the moment a second sharded generation existed.
  function shardedMode() {
    return app.schemaMode === "schema2" || app.schemaMode === "schema3";
  }

  //: The identity of a shard inside `loadedShardUrls`, `detailPromises` and friends. Schema 2
  //: names a shard by a content-addressed URL it also validates; schema 3 names it by a path that
  //: says what it is. One accessor rather than two branches at every call site, and it keeps the
  //: schema-2 URL validation exactly where it was.
  function shardKey(entry, where) {
    if (entry && typeof entry === "object" && typeof entry.path === "string") {
      return entry.path;
    }
    return immutableTimelineObjectUrl(entry, where);
  }

  function schema3SafeRelativePath(value, where) {
    var relative = text(value);
    if (relative.indexOf(SCHEMA_3_ROOT) !== 0 || relative.indexOf("..") >= 0 ||
        relative.indexOf("//") >= 0 || /[?#]/.test(relative)) {
      throw new Error(where + " must be a path under " + SCHEMA_3_ROOT + ".");
    }
    return relative;
  }

  function schema3Integer(value, where, minimum) {
    var parsed = number(value, NaN);
    if (!Number.isSafeInteger(parsed) || parsed < minimum) {
      throw new Error(where + " must be an integer of at least " + minimum + ".");
    }
    return parsed;
  }

  //: One catalogue entry, narrowed and given the two field names the rest of the application
  //: already speaks. `start_ms`/`end_ms` are not invented: they are `t0` and `t_end_exclusive`,
  //: which is exactly the pair the bootstrap tells a reader to compare a window against
  //: (`t0 < T1 and t_end_exclusive > T0`), and it is the same comparison `detailShardsForRange`
  //: has always made against schema 2. Renaming here rather than branching there keeps one
  //: selection rule in the application instead of one per generation.
  function schema3ShardEntry(raw, stream, where) {
    if (!raw || typeof raw !== "object") {
      throw new Error(where + " must be an object.");
    }
    if (text(raw.stream) !== stream) {
      throw new Error(where + " belongs to stream " + text(raw.stream) + ".");
    }
    var path = schema3SafeRelativePath(raw.path, where + ".path");
    var indexPath = schema3SafeRelativePath(raw.index_path, where + ".index_path");
    if (indexPath !== path + SCHEMA_3_INDEX_SUFFIX) {
      throw new Error(where + ".index_path must be its shard plus " + SCHEMA_3_INDEX_SUFFIX + ".");
    }
    var lineRanges = new Map();
    if (hasField(raw, "line_ranges")) {
      var ranges = raw.line_ranges && typeof raw.line_ranges === "object" ? raw.line_ranges : null;
      if (!ranges) {
        throw new Error(where + ".line_ranges must be an object.");
      }
      Object.keys(ranges).forEach(function (kind) {
        var pair = ranges[kind];
        if (!Array.isArray(pair) || pair.length !== 2) {
          throw new Error(where + ".line_ranges." + kind + " must be [first, count].");
        }
        lineRanges.set(kind, {
          first: schema3Integer(pair[0], where + ".line_ranges." + kind + "[0]", 0),
          count: schema3Integer(pair[1], where + ".line_ranges." + kind + "[1]", 0)
        });
      });
    }
    var t0 = Number.isFinite(raw.t0) ? Number(raw.t0) : null;
    var reach = Number.isFinite(raw.t_end_exclusive) ? Number(raw.t_end_exclusive) : null;
    return {
      stream: stream,
      team: typeof raw.team === "string" ? raw.team : null,
      day: typeof raw.day === "string" ? raw.day : null,
      path: path,
      index_path: indexPath,
      records: schema3Integer(raw.records, where + ".records", 0),
      members: schema3Integer(raw.members, where + ".members", 0),
      c_bytes: schema3Integer(raw.c_bytes, where + ".c_bytes", 0),
      u_bytes: schema3Integer(raw.u_bytes, where + ".u_bytes", 0),
      c_sha256: text(raw.c_sha256),
      u_sha256: text(raw.u_sha256),
      timestamps_sorted: raw.timestamps_sorted === true,
      start_ms: t0,
      end_ms: reach,
      line_ranges: lineRanges
    };
  }

  function schema3Stream(bootstrap, name, required) {
    var streams = bootstrap && typeof bootstrap.streams === "object" && bootstrap.streams
      ? bootstrap.streams
      : null;
    if (!streams) {
      throw new Error("Schema-3 bootstrap has no streams.");
    }
    var stream = streams[name] && typeof streams[name] === "object" ? streams[name] : null;
    if (!stream) {
      if (required) {
        throw new Error("Schema-3 bootstrap does not publish the " + name + " stream.");
      }
      return [];
    }
    var seen = new Set();
    return array(stream.shards).map(function (raw, index) {
      var entry = schema3ShardEntry(raw, name, "Schema-3 " + name + " shard " + index);
      if (seen.has(entry.path)) {
        throw new Error("Schema-3 " + name + " stream names " + entry.path + " twice.");
      }
      seen.add(entry.path);
      return entry;
    });
  }

  // -- the sidecar ------------------------------------------------------------------------

  //: Parse one `.index.jsonl` sidecar: a header line, then one line per member.
  //:
  //: Split on "\n" rather than with `String.prototype.split(/\r?\n/)` or any line-terminator
  //: regex, for the reason `ChunkIndex._parse` gives about `str.splitlines()`: the sidecar is
  //: written with `ensure_ascii=False`, so a `data_file` carrying U+2028 or U+0085 is one line on
  //: disk and would become two under a reader with a looser idea of what ends a line.
  function parseChunkIndex(source, entry) {
    var where = entry.index_path;
    var lines = text(source).split("\n").filter(function (line) { return line !== ""; });
    if (!lines.length) {
      throw new Error(where + " is empty.");
    }
    var header;
    try {
      header = JSON.parse(lines[0]);
    } catch (_error) {
      throw new Error(where + " header is not JSON.");
    }
    if (!header || typeof header !== "object" ||
        text(header.format) !== SCHEMA_3_INDEX_FORMAT ||
        number(header.version, NaN) !== SCHEMA_3_INDEX_VERSION) {
      throw new Error(where + " is not a version-1 " + SCHEMA_3_INDEX_FORMAT + ".");
    }
    if (text(header.codec) !== "gzip") {
      throw new Error(where + " uses codec " + text(header.codec) + ", which this page cannot read.");
    }
    // Bind the sidecar to the catalogue before any data byte is requested. Every one of these is
    // a fact the bootstrap already stated, so a mismatch means the two files came from different
    // builds -- and a stale sidecar does not give a slow answer, it gives a confident wrong one.
    if (number(header.c_size, NaN) !== entry.c_bytes ||
        number(header.u_size, NaN) !== entry.u_bytes ||
        number(header.record_count, NaN) !== entry.records ||
        number(header.member_count, NaN) !== entry.members ||
        text(header.c_sha256) !== entry.c_sha256 ||
        text(header.u_sha256) !== entry.u_sha256) {
      throw new Error(where + " describes a different generation than " + SCHEMA_3_URL + ".");
    }
    if (lines.length - 1 !== entry.members) {
      throw new Error(where + " lists " + (lines.length - 1) + " members, expected " + entry.members + ".");
    }
    var members = [];
    var compressed = 0;
    var uncompressed = 0;
    var line = 0;
    for (var index = 1; index < lines.length; index += 1) {
      var raw;
      try {
        raw = JSON.parse(lines[index]);
      } catch (_error) {
        throw new Error(where + " member " + (index - 1) + " is not JSON.");
      }
      if (!raw || typeof raw !== "object") {
        throw new Error(where + " member " + (index - 1) + " is not an object.");
      }
      var member = {
        c_off: schema3Integer(raw.c_off, where + " member " + (index - 1) + ".c_off", 0),
        c_len: schema3Integer(raw.c_len, where + " member " + (index - 1) + ".c_len", 1),
        u_off: schema3Integer(raw.u_off, where + " member " + (index - 1) + ".u_off", 0),
        u_len: schema3Integer(raw.u_len, where + " member " + (index - 1) + ".u_len", 0),
        l0: schema3Integer(raw.l0, where + " member " + (index - 1) + ".l0", 0),
        n: schema3Integer(raw.n, where + " member " + (index - 1) + ".n", 0),
        t0: Number.isFinite(raw.t0) ? Number(raw.t0) : null,
        t1: Number.isFinite(raw.t1) ? Number(raw.t1) : null
      };
      // Contiguity, checked once here rather than at every read that assumes it.
      if (member.c_off !== compressed || member.u_off !== uncompressed || member.l0 !== line) {
        throw new Error(where + " member " + (index - 1) + " is not contiguous.");
      }
      compressed += member.c_len;
      uncompressed += member.u_len;
      line += member.n;
      members.push(member);
    }
    if (compressed !== entry.c_bytes || uncompressed !== entry.u_bytes || line !== entry.records) {
      throw new Error(where + " member table does not cover its own header totals.");
    }
    return { header: header, members: members };
  }

  function loadChunkIndex(entry) {
    var url = entry.index_path;
    if (!app.shardIndexPromises.has(url)) {
      var request = (async function () {
        var response = await fetch(url, { credentials: "same-origin" });
        if (!response.ok) {
          var error = new Error("HTTP " + response.status + " for " + url);
          error.httpStatus = response.status;
          throw error;
        }
        return parseChunkIndex(await response.text(), entry);
      }());
      var cached = request.catch(function (error) {
        if (app.shardIndexPromises.get(url) === cached) {
          app.shardIndexPromises.delete(url);
        }
        throw error;
      });
      app.shardIndexPromises.set(url, cached);
    }
    return app.shardIndexPromises.get(url);
  }

  // -- member selection -------------------------------------------------------------------

  //: Members that can hold a record in the half-open window `[start, end)`.
  //:
  //: A member with no timestamped record (`t0 === null`) can never be selected by time, which is
  //: `ChunkIndexEntry.overlaps` restated: it is not "no constraint", it is "no record here has a
  //: position on the time axis". A `null` bound on the *query* side is open in that direction.
  function membersForTimeRange(members, start, end) {
    return members.filter(function (member) {
      if (member.t0 === null || member.t1 === null) {
        return false;
      }
      if (Number.isFinite(start) && member.t1 < start) {
        return false;
      }
      if (Number.isFinite(end) && member.t0 >= end) {
        return false;
      }
      return true;
    });
  }

  //: Members holding any line of `[first, first + count)`.
  function membersForLineRange(members, first, count) {
    if (count <= 0) {
      return [];
    }
    return members.filter(function (member) {
      return member.l0 < first + count && member.l0 + member.n > first;
    });
  }

  // -- reading one member -----------------------------------------------------------------

  //: Inflate one gzip member. Every fetched range is exactly one member, and a member is a
  //: complete gzip stream, so this never meets the concatenated case a `DecompressionStream`
  //: refuses -- which is the property the range read is here to produce.
  async function inflateGzipMember(buffer, where) {
    if (typeof DecompressionStream !== "function") {
      throw new Error(
        "This browser cannot inflate gzip (" + where + "). Schema 3 needs DecompressionStream."
      );
    }
    var stream = new Blob([buffer]).stream().pipeThrough(new DecompressionStream("gzip"));
    return await new Response(stream).arrayBuffer();
  }

  function parseContentRangeTotal(header, where) {
    var match = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(text(header).trim());
    if (!match) {
      throw new Error(where + " answered 206 with an unreadable Content-Range.");
    }
    return {
      first: Number(match[1]),
      last: Number(match[2]),
      total: Number(match[3])
    };
  }

  //: The page-wide gate on member fetches, and the whole of what makes SCHEMA_3_MEMBER_CONCURRENCY
  //: a page-wide bound.
  //:
  //: Deliberately wrapped around the *leaf* -- one member's request -- and around nothing else.
  //: A gate that also held sidecar fetches could deadlock: `readShardRecords` awaits the sidecar
  //: before it asks for a member, so six member fetches waiting on a seventh shard's sidecar,
  //: which is itself queued behind them, would never resolve. Member fetches wait for nothing
  //: that waits for a slot, so the queue below always drains.
  var schema3MemberSlots = { active: 0, waiting: [] };

  function schema3MemberSlot(run) {
    function release() {
      var next = schema3MemberSlots.waiting.shift();
      if (next) {
        next();
      } else {
        schema3MemberSlots.active -= 1;
      }
    }
    function start() {
      var settled;
      try {
        settled = Promise.resolve(run());
      } catch (error) {
        release();
        return Promise.reject(error);
      }
      return settled.then(function (value) {
        release();
        return value;
      }, function (error) {
        release();
        throw error;
      });
    }
    if (schema3MemberSlots.active < SCHEMA_3_MEMBER_CONCURRENCY) {
      schema3MemberSlots.active += 1;
      return start();
    }
    return new Promise(function (resolve, reject) {
      schema3MemberSlots.waiting.push(function () {
        start().then(resolve, reject);
      });
    });
  }

  //: Fetch, inflate and parse one member, once per page load.
  //:
  //: The `If-Range` on every request after the first is the point of remembering the ETag: a
  //: rebuild while the page is open replaces the shard, and a reader that spliced member 7 of the
  //: new file onto member 3 of the old one would produce records that never coexisted. The server
  //: answers a mismatched `If-Range` with the whole representation, which this detects and turns
  //: into a refusal rather than silently accepting a different generation's bytes.
  function fetchShardMember(entry, member) {
    var key = entry.path + "#" + member.c_off;
    if (!app.memberPromises.has(key)) {
      var request = schema3MemberSlot(async function () {
        var last = member.c_off + member.c_len - 1;
        var headers = { Range: "bytes=" + member.c_off + "-" + last };
        var known = app.shardEtags.get(entry.path);
        if (known) {
          headers["If-Range"] = known;
        }
        var response = await fetch(entry.path, {
          credentials: "same-origin",
          headers: headers
        });
        if (response.status === 416) {
          throw new Error(
            entry.path + " is shorter than " + SCHEMA_3_URL + " says; rebuild or reload."
          );
        }
        if (!response.ok) {
          var error = new Error("HTTP " + response.status + " for " + entry.path);
          error.httpStatus = response.status;
          throw error;
        }
        var etag = text(response.headers.get("ETag"));
        var buffer = await response.arrayBuffer();
        if (response.status === 206) {
          var range = parseContentRangeTotal(
            response.headers.get("Content-Range"),
            entry.path
          );
          if (range.total !== entry.c_bytes || range.first !== member.c_off ||
              range.last !== last) {
            throw new Error(
              entry.path + " served bytes " + range.first + "-" + range.last + "/" +
                range.total + ", not the member " + SCHEMA_3_URL + " described."
            );
          }
        } else {
          // A 200 to a range request is always legal -- a server may ignore `Range` -- so the
          // whole file is a correct answer and the member is sliced out of it here. What is not
          // acceptable is a 200 that arrived because `If-Range` did *not* match, which is the
          // one case where the bytes belong to a different version of the shard.
          if (known && etag && etag !== known) {
            throw new Error(
              entry.path + " changed while it was being read; reload the page."
            );
          }
          if (buffer.byteLength !== entry.c_bytes) {
            throw new Error(
              entry.path + " is " + buffer.byteLength + " bytes, not the " + entry.c_bytes +
                " " + SCHEMA_3_URL + " described."
            );
          }
          buffer = buffer.slice(member.c_off, member.c_off + member.c_len);
        }
        if (etag && !app.shardEtags.has(entry.path)) {
          app.shardEtags.set(entry.path, etag);
        }
        var inflated = await inflateGzipMember(buffer, entry.path);
        if (inflated.byteLength !== member.u_len) {
          throw new Error(
            entry.path + " member at " + member.c_off + " inflated to " + inflated.byteLength +
              " bytes, expected " + member.u_len + "."
          );
        }
        var body = new TextDecoder("utf-8", { fatal: true }).decode(inflated);
        var lines = body.split("\n");
        if (lines.length && lines[lines.length - 1] === "") {
          lines.pop();
        }
        if (lines.length !== member.n) {
          throw new Error(
            entry.path + " member at " + member.c_off + " holds " + lines.length +
              " lines, expected " + member.n + "."
          );
        }
        return lines.map(function (line, offset) {
          var record;
          try {
            record = JSON.parse(line);
          } catch (_error) {
            throw new Error(entry.path + " line " + (member.l0 + offset) + " is not JSON.");
          }
          if (!record || typeof record !== "object" || Array.isArray(record)) {
            throw new Error(entry.path + " line " + (member.l0 + offset) + " is not an object.");
          }
          return record;
        });
      });
      var cached = request.catch(function (error) {
        if (app.memberPromises.get(key) === cached) {
          app.memberPromises.delete(key);
        }
        throw error;
      });
      app.memberPromises.set(key, cached);
    }
    return app.memberPromises.get(key);
  }

  //: Run *jobs* with a bounded number in flight, preserving their order in the result.
  async function boundedAll(jobs, limit) {
    var results = new Array(jobs.length);
    var next = 0;
    async function worker() {
      while (next < jobs.length) {
        var index = next;
        next += 1;
        results[index] = await jobs[index]();
      }
    }
    var workers = [];
    for (var slot = 0; slot < Math.min(limit, jobs.length); slot += 1) {
      workers.push(worker());
    }
    await Promise.all(workers);
    return results;
  }

  //: Every record of *entry* that a selector admits, in shard order.
  //:
  //: `selector` is either `{ start_ms, end_ms }` (time-addressed streams) or
  //: `{ first, count }` (line-addressed ones: the spine and the relationship sidecar). Both first
  //: choose members and then filter *within* them, because a member is the smallest unit the
  //: format can seek to and a day boundary does not fall on one.
  async function readShardRecords(entry, selector) {
    var index = await loadChunkIndex(entry);
    var byLine = hasField(selector, "first");
    var members = byLine
      ? membersForLineRange(index.members, selector.first, selector.count)
      : membersForTimeRange(index.members, selector.start_ms, selector.end_ms);
    var pages = await boundedAll(
      members.map(function (member) {
        return function () {
          return fetchShardMember(entry, member).then(function (records) {
            return { member: member, records: records };
          });
        };
      }),
      SCHEMA_3_MEMBER_CONCURRENCY
    );
    var selected = [];
    pages.forEach(function (page) {
      page.records.forEach(function (record, offset) {
        if (byLine) {
          var line = page.member.l0 + offset;
          if (line < selector.first || line >= selector.first + selector.count) {
            return;
          }
        } else {
          var at = number(record.at_ms, NaN);
          if (!Number.isFinite(at) ||
              (Number.isFinite(selector.start_ms) && at < selector.start_ms) ||
              (Number.isFinite(selector.end_ms) && at >= selector.end_ms)) {
            return;
          }
        }
        selected.push(record);
      });
    });
    return selected;
  }

  //: The whole of one line-addressed kind, or an empty list when the shard does not carry it.
  function readShardKind(entry, kind) {
    var range = entry.line_ranges.get(kind);
    if (!range || !range.count) {
      return Promise.resolve([]);
    }
    return readShardRecords(entry, { first: range.first, count: range.count });
  }

  function schema3RecordsOfKind(records, kind) {
    return records.filter(function (record) {
      return text(record.record_kind) === kind;
    });
  }

  //: Strip the one key the schema-3 envelope adds, so that what reaches the rest of the page is
  //: the schema-1 record it always was. `at_ms` is deliberately *not* stripped: an event carried
  //: it in schema 1 and the writer asserts the two agree, and a phase or edge that gained one is
  //: carrying a field no renderer reads.
  function schema3Payload(record) {
    var copy = {};
    Object.keys(record).forEach(function (key) {
      if (key !== "record_kind") {
        copy[key] = record[key];
      }
    });
    return copy;
  }

  // -- the spine, which is the first paint --------------------------------------------------

  //: The line ranges a first paint needs from one spine shard, and the two kinds it skips.
  //:
  //: The spine lays its kinds down in a declared order, and the two the first frame does *not*
  //: need are the two largest: `activity_bounds` is last, and `phase_card` sits in the middle. So
  //: this is `[0, l0(phase_card))` and `[l0(structural_edge), l0(activity_bounds))` -- two
  //: contiguous ranges with a hole between them, each of which is a run of members and therefore
  //: one `Range` request per member.
  //:
  //: Measured on the twelve-team archive, against reading the whole prefix up to the zoom bounds:
  //: **2,333,892 compressed bytes down to 1,993,057, and 18,275,931 bytes of JSON to parse down to
  //: 15,129,075.** The hole is worth a second range because it is 14% of the transfer and 17% of
  //: the parse, and because it is what makes the first paint strictly cheaper than schema 2's on
  //: *both* axes rather than only on transfer -- schema 2 defers its phase cards too, into the
  //: separate `timeline-phase-index` object, so a schema 3 that loaded them eagerly would have
  //: been comparing two different amounts of work.
  //:
  //: Skipping `phase_card` is safe for exactly the reason schema 2's arrangement is: no renderer
  //: reads a card at load. `app.data.phases` is empty under both generations, the full phases
  //: arrive from the day shards as the view asks for them, and the cards have one consumer --
  //: `loadPhaseIndex`, behind the agent-lifetime modal -- which is asynchronous already and shows
  //: a loading state. `ensurePhaseCards` is that fetch.
  //:
  //: Asking for the eight leading kinds separately, rather than as runs, would name the same lines
  //: in eight calls; asking for the whole shard would drag the archive's 324,624 bytes of zoom
  //: bounds across for a page that may never zoom.
  function spineFirstPaintRanges(entry) {
    var bounds = entry.line_ranges.get("activity_bounds");
    var end = bounds ? bounds.first : entry.records;
    var cards = entry.line_ranges.get("phase_card");
    if (!cards || !cards.count) {
      return [{ first: 0, count: end }];
    }
    var after = cards.first + cards.count;
    var ranges = [{ first: 0, count: cards.first }];
    if (end > after) {
      ranges.push({ first: after, count: end - after });
    }
    return ranges.filter(function (range) { return range.count > 0; });
  }

  //: The phase cards of one team, fetched the first time something wants the phase index.
  function ensurePhaseCards(team) {
    var entry = app.spineByTeam.get(team);
    if (!schema3Enabled() || !entry) {
      return Promise.resolve(null);
    }
    if (!app.phaseCardPromises.has(team)) {
      var request = readShardKind(entry, "phase_card").then(function (records) {
        records.map(schema3Payload).forEach(installPhaseCard);
        sortPhaseIndex();
        return records;
      });
      var cached = request.catch(function (error) {
        if (app.phaseCardPromises.get(team) === cached) {
          app.phaseCardPromises.delete(team);
        }
        throw error;
      });
      app.phaseCardPromises.set(team, cached);
    }
    return app.phaseCardPromises.get(team);
  }

  function installPhaseCard(card) {
    var agentId = text(card.agent_id);
    if (!agentId) {
      throw new Error("Schema-3 phase card has no agent.");
    }
    if (!app.phaseIndexByAgent.has(agentId)) {
      app.phaseIndexByAgent.set(agentId, []);
    }
    app.phaseIndexByAgent.get(agentId).push(card);
  }

  function sortPhaseIndex() {
    app.phaseIndexByAgent.forEach(function (cards) {
      cards.sort(function (left, right) {
        return number(left.start_ms, 0) - number(right.start_ms, 0) ||
          text(left.id).localeCompare(text(right.id));
      });
    });
  }

  function schema3InstallSpine(team, records, sink) {
    records.forEach(function (record) {
      var kind = text(record.record_kind);
      var payload = schema3Payload(record);
      if (kind === "team") {
        return;
      }
      if (kind === "agent") {
        sink.agents.push(payload);
      } else if (kind === "phase_card") {
        // Only reachable on a shard whose cards happened to share a member with a kind the frame
        // does need; `spineFirstPaintRanges` does not ask for them. Kept rather than refused
        // because a member is the unit of transfer, so over-reading is normal and dropping the
        // records would mean fetching them again.
        sink.phaseCards.push(payload);
      } else if (kind === "structural_edge") {
        sink.edges.push(payload);
      } else if (kind === "rollup") {
        sink.rollups.push(payload);
      } else if (kind === "project") {
        sink.projects.push(payload);
      } else if (kind === "summary_file") {
        sink.summary_files.push(payload);
      } else if (kind === "glossary_term") {
        sink.glossary.push(payload);
      } else if (kind === "project_overview") {
        sink.project_overviews.push(payload);
      } else if (kind !== "activity_bounds") {
        throw new Error(
          "Schema-3 spine shard for " + team + " carries an unknown record kind " + kind + "."
        );
      }
    });
  }

  // -- the zoom bounds, fetched on the first zoom and not before ----------------------------

  function schema3LocalIdentifier(team, identifier) {
    var prefix = team + "::";
    return identifier.indexOf(prefix) === 0 ? identifier.slice(prefix.length) : identifier;
  }

  //: The stable reference an `activity_bounds` record is keyed by, for the three subjects the
  //: context menus can zoom to. The scope is what says which of the three this is: a phase scope
  //: carries `phase_id`, an agent scope carries only `agent_id`, and a rollup passes none --
  //: which is exactly how `zoomToActivityRange`'s callers already distinguish them, so nothing
  //: here has to guess from the shape of the record.
  //: Which team a presentation record belongs to, falling back to the sole published team.
  //:
  //: **A single-team render does not stamp `team` on anything but its agents.** There is only
  //: one team, schema 1 does not carry the field, and `render.py` adds it to agents and not to
  //: phases or rollups -- which is exactly why the schema-3 writer's `_team_of` has a
  //: `sole_team` fallback, and why `query._SchemaThreeArchive._unwrap` has the same fallback
  //: read from the other side. This is the third copy of that one rule, and it has to exist:
  //: without it every `activity_bounds` reference computed for a phase or a rollup on a
  //: single-team archive comes out empty, no published bound is ever found, and "Zoom to work
  //: phase" silently degrades to fetching every day shard the subject overlaps -- for a monthly
  //: rollup, the whole archive -- to recompute two numbers that are sitting in the spine.
  //: Schema 2 never showed this because it carried the bounds inline on the record itself.
  function boundsTeam(bounds) {
    var declared = text(bounds && bounds.team);
    if (declared) {
      return declared;
    }
    return app.spineByTeam.size === 1
      ? app.spineByTeam.keys().next().value
      : "";
  }

  function activityBoundsRef(bounds, scope) {
    var team = boundsTeam(bounds);
    if (!team) {
      return "";
    }
    if (scope && text(scope.phase_id)) {
      return "phase:" + team + "::" + schema3LocalIdentifier(team, text(bounds.id));
    }
    if (scope && text(scope.agent_id) && text(bounds.id)) {
      return "agent:" + team + "::" + schema3LocalIdentifier(team, text(bounds.id));
    }
    var kind = text(bounds && bounds.kind);
    var start = number(bounds && bounds.start_ms, NaN);
    if (!kind || !Number.isFinite(start)) {
      return "";
    }
    return "rollup:" + team + "::" + kind + "::" + start;
  }

  function ensureActivityBounds(team) {
    var entry = app.spineByTeam.get(team);
    if (!schema3Enabled() || !entry) {
      return Promise.resolve(null);
    }
    if (!app.activityBoundsPromises.has(team)) {
      var request = readShardKind(entry, "activity_bounds").then(function (records) {
        records.forEach(function (record) {
          var reference = text(record.ref);
          var start = number(record.activity_start_ms, NaN);
          var end = number(record.activity_end_ms, NaN);
          if (!reference || !Number.isFinite(start) || !Number.isFinite(end)) {
            throw new Error("Schema-3 activity bounds for " + team + " are incomplete.");
          }
          app.activityBoundsByRef.set(reference, {
            activity_start_ms: start,
            activity_end_ms: end
          });
        });
        return records;
      });
      var cached = request.catch(function (error) {
        if (app.activityBoundsPromises.get(team) === cached) {
          app.activityBoundsPromises.delete(team);
        }
        throw error;
      });
      app.activityBoundsPromises.set(team, cached);
    }
    return app.activityBoundsPromises.get(team);
  }

  // -- detail shards ------------------------------------------------------------------------

  //: Merge one schema-3 timeline shard. The records are the schema-1 events, phases and
  //: non-structural edges of one team's UTC day, so once the envelope key is dropped they are
  //: exactly what `mergeDetailShard` already knows how to install -- which is why this hands them
  //: to that function rather than growing a second merge with its own deduplication rules.
  function loadSchema3DetailShard(entry) {
    var key = entry.path;
    if (!app.detailPromises.has(key)) {
      var request = readShardRecords(entry, { start_ms: NaN, end_ms: NaN }).then(
        function (records) {
          var shard = {
            schema_version: 2,
            kind: "timeline-detail-day",
            phases: [],
            edges: [],
            events: []
          };
          records.forEach(function (record) {
            var kind = text(record.record_kind);
            var payload = schema3Payload(record);
            if (kind === "phase") {
              shard.phases.push(payload);
            } else if (kind === "edge") {
              shard.edges.push(payload);
            } else if (kind === "event") {
              shard.events.push(payload);
            } else {
              throw new Error(entry.path + " carries an unknown record kind " + kind + ".");
            }
          });
          mergeDetailShard(shard, key);
          return shard;
        }
      );
      var cached = request.catch(function (error) {
        if (app.detailPromises.get(key) === cached) {
          app.detailPromises.delete(key);
        }
        throw error;
      });
      app.detailPromises.set(key, cached);
    }
    return app.detailPromises.get(key);
  }

  // -- the transcript search corpus ----------------------------------------------------------

  function schema3DayRange(entry, where) {
    var match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(text(entry.day));
    if (!match) {
      throw new Error(where + " is not addressed by a UTC day.");
    }
    var start = Date.UTC(Number(match[1]), Number(match[2]) - 1, Number(match[3]));
    return { start_ms: start, end_ms: start + SCHEMA_3_DAY_MS };
  }

  //: The catalogue the search UI already speaks, projected from the schema-3 `search` stream.
  //: `counts.records` and `start_ms`/`end_ms` are the two fields `validateTranscriptSearchShard`
  //: and `searchShardAt` read, and giving them the same names is what lets one search
  //: implementation serve both generations.
  function schema3SearchCatalog(shards) {
    return shards.map(function (entry) {
      var range = schema3DayRange(entry, entry.path);
      return Object.assign({}, entry, {
        start_ms: range.start_ms,
        end_ms: range.end_ms,
        counts: { records: entry.records }
      });
    }).sort(function (left, right) {
      return text(left.team).localeCompare(text(right.team)) ||
        left.start_ms - right.start_ms;
    });
  }

  //: Load one team's prefilter, and only when a query has a term a trigram can be built from.
  //:
  //: This is the decision schema 2 could not make. Its blooms are inlined in the bootstrap, so
  //: every command paid 4,527,592 base64 characters whether or not it searched, and a two-byte
  //: query -- which cannot use a trigram filter at all -- paid for all of them and then skipped
  //: every filter. Here the filters are a stream, so `B3` reads none of it.
  //:
  //: The path a record names is checked against *this team's own* search shards, not merely
  //: parsed. Keying by path is what makes another team's shard addressable from here, and a
  //: Bloom filter's only wrong answer is a false miss -- so one team's filter installed under
  //: another team's path would make `searchShardMightMatch` skip that whole day and drop every
  //: record in it from the result, with nothing to say so. Schema 2 could not express this,
  //: because its filters are inlined on the catalogue entry they belong to. `query.py`'s
  //: `_SchemaThreeArchive.search_blooms` makes the same check on the same records.
  function ensureSearchBlooms(team) {
    var entry = app.searchBloomByTeam.get(team);
    if (!entry) {
      return Promise.resolve(null);
    }
    var owned = new Set();
    app.searchCatalog.forEach(function (shard) {
      if (text(shard.team) === team && text(shard.path)) {
        owned.add(text(shard.path));
      }
    });
    if (!app.searchBloomPromises.has(team)) {
      var request = readShardRecords(entry, { start_ms: NaN, end_ms: NaN }).then(
        function (records) {
          records.forEach(function (record) {
            var shard = text(record.shard);
            if (!shard || text(record.team) !== team) {
              throw new Error(entry.path + " has a prefilter record for another team.");
            }
            if (!owned.has(shard)) {
              throw new Error(
                entry.path + " has a prefilter for " + shard +
                  ", which is not a search shard of " + team + "."
              );
            }
            app.searchBloomByUrl.set(
              shard,
              decodeTrigramBloom(record.bloom, "transcript search prefilter " + shard)
            );
          });
          return records;
        }
      );
      var cached = request.catch(function (error) {
        if (app.searchBloomPromises.get(team) === cached) {
          app.searchBloomPromises.delete(team);
        }
        throw error;
      });
      app.searchBloomPromises.set(team, cached);
    }
    return app.searchBloomPromises.get(team);
  }

  //: Whether the query can be pruned with at all, which is what decides whether the prefilter is
  //: worth a fetch. `queryTermBloomEligible` is the same test the filter itself applies.
  function queryCanUseBloom(query) {
    return searchQueryParts(query).some(function (part) {
      return queryTermBloomEligible(part.value);
    });
  }

  function schema3SearchTeams() {
    var teams = [];
    app.searchCatalog.forEach(function (shard) {
      var team = text(shard.team);
      if (team && teams.indexOf(team) < 0 &&
          (!app.selectedTeam || team === app.selectedTeam)) {
        teams.push(team);
      }
    });
    return teams;
  }

  //: One team's relationship sidecar: prompt excerpts, then response->prompt edges, each a
  //: published line range. A search that matched no prompt reads only the response half.
  function ensureSearchLinks(team) {
    var entry = app.searchLinksByTeam.get(team);
    if (!entry) {
      return Promise.resolve(null);
    }
    if (!app.searchLinkPromises.has(team)) {
      var request = Promise.all([
        readShardKind(entry, "search_prompt"),
        readShardKind(entry, "search_response")
      ]).then(function (halves) {
        var messagePrefix = "message:" + team + "::";
        var agentPrefix = "agent:" + team + "::";
        halves[0].forEach(function (record) {
          var reference = text(record.ref);
          if (reference.indexOf(messagePrefix) !== 0 || typeof record.excerpt !== "string") {
            throw new Error(entry.path + " has an invalid prompt excerpt.");
          }
          app.searchLinkPromptRefs.add(reference);
          app.searchPromptExcerpts.set(reference, record.excerpt);
        });
        halves[1].forEach(function (record) {
          var reference = text(record.ref);
          var promptReference = text(record.prompt_ref);
          var at = number(record.at_ms, NaN);
          if (reference.indexOf(messagePrefix) !== 0 ||
              promptReference.indexOf(messagePrefix) !== 0 ||
              text(record.agent_ref).indexOf(agentPrefix) !== 0 ||
              !Number.isFinite(at)) {
            throw new Error(entry.path + " has an invalid response link.");
          }
          app.searchLinkResponseRefs.add(reference);
          if (!app.searchResponsesByPrompt.has(promptReference)) {
            app.searchResponsesByPrompt.set(promptReference, []);
          }
          app.searchResponsesByPrompt.get(promptReference).push(schema3Payload(record));
        });
        app.searchResponsesByPrompt.forEach(function (responses) {
          responses.sort(function (left, right) {
            return number(left.at_ms, 0) - number(right.at_ms, 0) ||
              text(left.ref).localeCompare(text(right.ref));
          });
        });
        app.loadedSearchLinkUrls.add(entry.path);
        return halves;
      });
      var cached = request.catch(function (error) {
        if (app.searchLinkPromises.get(team) === cached) {
          app.searchLinkPromises.delete(team);
        }
        throw error;
      });
      app.searchLinkPromises.set(team, cached);
    }
    return app.searchLinkPromises.get(team);
  }

  function loadSchema3SearchShard(entry) {
    var key = entry.path;
    if (app.loadedSearchShardUrls.has(key)) {
      return Promise.resolve(null);
    }
    return readShardRecords(entry, { start_ms: NaN, end_ms: NaN }).then(function (records) {
      var payloads = records.map(schema3Payload);
      var shard = {
        schema_version: 1,
        kind: "timeline-search-day",
        team: entry.team,
        range: { start_ms: entry.start_ms, end_ms: entry.end_ms },
        records: payloads
      };
      // No `source_digest` on the fabricated envelope. Schema 2 puts one on every object and
      // `validateSchema2ObjectSourceDigest` binds it to the generation; copying the page's own
      // digest onto a record that never carried one would be a check against itself. Schema 3's
      // equivalent binding is stronger and has already run by here: the sidecar's digests and
      // sizes were matched against the bootstrap before the first byte of this shard was fetched.
      mergeTranscriptSearchShard(shard, entry, key);
      return shard;
    });
  }

  // -- loading the generation ------------------------------------------------------------------

  async function loadSchema3() {
    var bootstrap;
    try {
      bootstrap = await fetchJsonCached(SCHEMA_3_URL);
    } catch (error) {
      if (error && error.httpStatus === 404) {
        return false;
      }
      throw error;
    }
    if (!bootstrap || typeof bootstrap !== "object" ||
        number(bootstrap.schema_version, NaN) !== 3 ||
        text(bootstrap.kind) !== "timeline-v3-bootstrap") {
      throw new Error("Unsupported schema-3 timeline bootstrap.");
    }
    var codec = bootstrap.codec && typeof bootstrap.codec === "object" ? bootstrap.codec : null;
    if (!codec || text(codec.container) !== "multi-member-gzip" ||
        text(codec.timestamp_key) !== "at_ms" ||
        text(codec.record_kind_key) !== "record_kind" ||
        text(codec.index_suffix) !== SCHEMA_3_INDEX_SUFFIX) {
      throw new Error("Schema-3 bootstrap declares a codec this page cannot read.");
    }
    if (typeof DecompressionStream !== "function") {
      // Declined rather than attempted, so `loadTimeline` falls through to schema 2 with a
      // reason a reader can act on instead of a stack trace from the first member.
      throw new Error("This browser has no DecompressionStream; schema 3 cannot be read.");
    }
    var teamSlugs = new Set();
    array(bootstrap.teams).forEach(function (team) {
      var slug = text(team && team.slug);
      if (!slug || teamSlugs.has(slug)) {
        throw new Error("Schema-3 timeline bootstrap has an invalid or duplicate team.");
      }
      teamSlugs.add(slug);
    });
    if (!teamSlugs.size) {
      throw new Error("Schema-3 timeline bootstrap does not identify a team.");
    }
    var range = bootstrap.range && typeof bootstrap.range === "object" ? bootstrap.range : null;
    var rangeStart = number(range && range.start_ms, NaN);
    var rangeEnd = number(range && range.end_ms, NaN);
    if (!Number.isFinite(rangeStart) || !Number.isFinite(rangeEnd) || rangeEnd <= rangeStart) {
      throw new Error("Schema-3 timeline bootstrap has an invalid range.");
    }

    var timelineShards = schema3Stream(bootstrap, "timeline", true);
    var spineShards = schema3Stream(bootstrap, "spine", true);
    var binsShards = schema3Stream(bootstrap, "bins", true);
    var searchShards = schema3Stream(bootstrap, "search", false);
    var bloomShards = schema3Stream(bootstrap, "search_bloom", false);
    var linkShards = schema3Stream(bootstrap, "search_links", false);
    // All three streams or none, *and* all three for every team that has a `search` shard.
    //
    // Two checks rather than one, because they catch two different half-publications and the
    // first does not imply the second. The stream rule says the *format* is whole: a corpus
    // with no prefilter would silently over-read, and one with no relationships would answer
    // with linkage it cannot see. The per-team rule says the *corpus* is whole: a build that
    // died between two teams leaves all three sections non-empty and one team's relationships
    // missing, and a search over that team would then render every linked response as unlinked
    // and every matched prompt with no excerpt and no reply count -- an ordinary-looking answer
    // that nothing downstream can detect. `query._SchemaThreeArchive._check_search_corpus` is
    // the same pair of rules on the same catalogue, and the two readers have to agree: a
    // generation the CLI declines and the page accepts is a generation on which the two
    // surfaces answer the same question differently.
    var searchStreamCount = (searchShards.length ? 1 : 0) + (bloomShards.length ? 1 : 0) +
      (linkShards.length ? 1 : 0);
    if (searchStreamCount !== 0 && searchStreamCount !== 3) {
      throw new Error("Schema-3 bootstrap publishes a partial transcript search corpus.");
    }
    [
      { stream: "search_bloom", shards: bloomShards },
      { stream: "search_links", shards: linkShards }
    ].forEach(function (side) {
      var published = new Set(side.shards.map(function (entry) { return entry.team; }));
      searchShards.forEach(function (entry) {
        if (!published.has(entry.team)) {
          throw new Error(
            "The transcript search corpus has no " + side.stream + " shard for " + entry.team +
              "."
          );
        }
      });
    });
    spineShards.forEach(function (entry) {
      if (!entry.team || !teamSlugs.has(entry.team)) {
        throw new Error("Schema-3 spine shard " + entry.path + " names an unpublished team.");
      }
    });
    timelineShards.forEach(function (entry) {
      if (!entry.team || !teamSlugs.has(entry.team)) {
        throw new Error("Schema-3 timeline shard " + entry.path + " names an unpublished team.");
      }
      if (entry.start_ms === null || entry.end_ms === null) {
        throw new Error("Schema-3 timeline shard " + entry.path + " publishes no time range.");
      }
    });

    app.spineByTeam = new Map(spineShards.map(function (entry) {
      return [entry.team, entry];
    }));
    app.searchBloomByTeam = new Map(bloomShards.map(function (entry) {
      return [entry.team, entry];
    }));
    app.searchLinksByTeam = new Map(linkShards.map(function (entry) {
      return [entry.team, entry];
    }));

    // One parallel round: every team's spine prefix, and the single bins shard.
    //
    // The bins go with the spine rather than after it. `timeline_v3` moved them out of the
    // bootstrap because they grow as days x teams x resolutions and a bootstrap whose size is set
    // by its largest pre-aggregation is not a bootstrap -- that argument is about what gates the
    // *first* fetch, not about deferring a second one. They are 65,384 bytes, they are the
    // aggregate chart, and fetching them beside the spine costs no extra round trip while
    // deferring them would paint an empty chart and then move it under the reader.
    var sink = {
      agents: [],
      phaseCards: [],
      edges: [],
      rollups: [],
      projects: [],
      summary_files: [],
      glossary: [],
      project_overviews: []
    };
    var loaded = await Promise.all([
      Promise.all(spineShards.map(function (entry) {
        return Promise.all(
          spineFirstPaintRanges(entry).map(function (range) {
            return readShardRecords(entry, range);
          })
        ).then(function (pages) {
          return {
            team: entry.team,
            records: pages.reduce(function (all, page) { return all.concat(page); }, [])
          };
        });
      })),
      binsShards.length
        ? readShardRecords(binsShards[0], { start_ms: NaN, end_ms: NaN })
        : Promise.resolve([])
    ]);
    loaded[0].forEach(function (shard) {
      schema3InstallSpine(shard.team, shard.records, sink);
    });

    app.schemaMode = "schema3";
    app.shardCatalog = timelineShards.slice().sort(function (left, right) {
      return left.start_ms - right.start_ms ||
        text(left.team).localeCompare(text(right.team));
    });
    app.searchCatalog = schema3SearchCatalog(searchShards);
    app.searchBloomByUrl = new Map();
    app.detailPromises.clear();
    app.loadedShardUrls.clear();
    app.loadedSearchShardUrls.clear();
    app.loadedSearchLinkUrls.clear();
    app.searchRecords = [];
    app.searchRecordsByRef.clear();
    app.searchPromptExcerpts.clear();
    app.searchResponsesByPrompt.clear();
    app.searchLinkPromptRefs.clear();
    app.searchLinkResponseRefs.clear();
    app.activityBoundsByRef.clear();
    app.activityBoundsPromises.clear();
    app.searchBloomPromises.clear();
    app.searchLinkPromises.clear();
    app.searchShardState = app.shardCatalog.length ? "unloaded" : "ready";
    app.searchCorpusMode = app.searchCatalog.length ? "schema3" : "none";
    app.searchCorpusNote = "";
    if (app.searchCorpusMode === "none") {
      var declined = await schema3Schema2SearchFallback(bootstrap);
      // Three outcomes, two of which the reader is told about. "There is no corpus anywhere" is
      // the ordinary state of an archive exported without one and says nothing worth a line;
      // "the corpus came from the older generation" and "there is a corpus and it was refused"
      // are both surprises, and the second is the one an operator has to act on.
      if (!declined) {
        app.searchCorpusNote =
          "transcript search from schema 2 (this archive predates the schema-3 corpus)";
      } else if (declined !== SCHEMA_2_CORPUS_ABSENT) {
        app.searchCorpusNote = "transcript search unavailable (" + declined + ")";
      }
    }
    app.transcriptSearchState = app.searchCatalog.length ? "unloaded" : "unavailable";
    app.transcriptSearchError = app.searchCatalog.length
      ? ""
      : "This export does not contain transcript search shards.";

    // The spine's `phase_card` kind *is* schema 2's phase index: the same nine fields, one record
    // per phase, no `states` array. So it is installed the same way and the modal that used to
    // fetch a separate content-addressed object now finds it already in hand -- one fewer object
    // in the archive and one fewer round trip on the first agent-lifetime open.
    app.phaseIndexReference = null;
    app.phaseIndexPromise = null;
    app.phaseIndexByAgent = new Map();
    app.phaseCardPromises.clear();
    // Not "ready": the cards are a line range this paint deliberately skipped, and `loadPhaseIndex`
    // fetches them. Any card that arrived anyway -- because it shared a member with a kind the
    // frame did need -- is installed here so it is not fetched twice.
    app.phaseIndexReady = false;
    sink.phaseCards.forEach(installPhaseCard);
    sortPhaseIndex();

    var merged = {
      schema_version: 3,
      generated_at: bootstrap.generated_at,
      source_digest: bootstrap.source_digest,
      display_timezone: bootstrap.display_timezone,
      display_timezone_source: bootstrap.display_timezone_source,
      range: bootstrap.range,
      stats: bootstrap.stats,
      artifact_catalog_path: bootstrap.artifact_catalog_path,
      glossary_path: bootstrap.glossary_path,
      teams: bootstrap.teams,
      activity_bins: loaded[1].map(schema3Payload),
      agents: sink.agents,
      rollups: sink.rollups,
      projects: sink.projects,
      summary_files: sink.summary_files,
      glossary: sink.glossary,
      edges: sink.edges,
      // Empty for the same reason schema 2 empties them: the full records live in the day shards
      // and arrive as the view asks for them. The cards are in `phaseIndexByAgent` above.
      phases: [],
      events: []
    };
    if (sink.project_overviews.length === 1) {
      merged.project_overview = sink.project_overviews[0];
    }
    if (sink.project_overviews.length) {
      merged.project_overviews = sink.project_overviews;
    }
    initializeData(merged);
    return true;
  }

  //: The transcript search corpus a schema-2 bootstrap publishes: the day-shard catalogue and the
  //: Bloom filters inlined beside it, keyed by the content-addressed URL the page will fetch.
  //:
  //: Factored out of `loadSchema2` because it now has a second caller. An archive built after the
  //: schema-3 *timeline* and before the schema-3 *search streams* has a corpus here and nowhere
  //: else, and `loadSchema3` reads it rather than declaring search unavailable -- see
  //: `schema3Schema2SearchFallback`. One implementation rather than two, because the two callers
  //: must agree about which shards exist and what their filters say; a second parser would be a
  //: second answer to that question.
  function schema2SearchCatalog(bootstrap) {
    var blooms = new Map();
    var config = bootstrap && bootstrap.search && typeof bootstrap.search === "object"
      ? bootstrap.search
      : null;
    if (!config || text(config.strategy) !== "transcript-message-shards") {
      return { catalog: [], blooms: blooms };
    }
    if (number(config.schema_version, NaN) !== 1) {
      throw new Error("Unsupported transcript search catalog schema.");
    }
    var seen = new Set();
    var catalog = array(config.shards).slice();
    catalog.forEach(function (shard) {
      var url = immutableTimelineObjectUrl(shard, "transcript search shard");
      var team = text(shard.team);
      var start = number(shard.start_ms, NaN);
      var end = number(shard.end_ms, NaN);
      var recordCount = transcriptSearchRecordCount(shard);
      if (!team || !Number.isFinite(start) || !Number.isFinite(end) || end <= start ||
          !Number.isFinite(recordCount) || seen.has(url)) {
        throw new Error("Invalid or duplicate transcript search shard catalog entry.");
      }
      if (hasField(shard, "linkage")) {
        var linkage = shard.linkage && typeof shard.linkage === "object"
          ? shard.linkage
          : null;
        var linkageCounts = linkage && linkage.counts &&
          typeof linkage.counts === "object" ? linkage.counts : null;
        immutableTimelineObjectUrl(linkage, "transcript search linkage shard");
        if (!Number.isSafeInteger(number(linkageCounts && linkageCounts.prompts, NaN)) ||
            number(linkageCounts && linkageCounts.prompts, NaN) < 0 ||
            !Number.isSafeInteger(number(linkageCounts && linkageCounts.responses, NaN)) ||
            number(linkageCounts && linkageCounts.responses, NaN) < 0) {
          throw new Error("Invalid transcript search linkage catalog entry.");
        }
      }
      if (hasField(shard, "trigram_bloom")) {
        blooms.set(
          url,
          decodeTrigramBloom(
            shard.trigram_bloom,
            "transcript search shard " + team + " " + text(shard.day, url) + ".trigram_bloom"
          )
        );
      }
      seen.add(url);
    });
    catalog.sort(function (left, right) {
      return text(left.team).localeCompare(text(right.team)) ||
        number(left.start_ms, 0) - number(right.start_ms, 0);
    });
    return { catalog: catalog, blooms: blooms };
  }

  //: The corpus a schema-3 archive that predates the search streams still has: schema 2's.
  //:
  //: **This is a capability, not a courtesy.** The bundle is *copied into the archive it builds*,
  //: so the two halves of an archive move independently: a build that publishes a schema-3
  //: timeline without the search streams -- because it predates them, or because it died between
  //: copying the bundle and publishing them -- leaves a tree whose `data/timeline-v2.json` lists
  //: a full corpus that this page would otherwise refuse to look at, while `./timeline search` on
  //: the same directory answers from it happily. A graphical surface that silently loses a
  //: capability the command line still has, and says "this export does not contain transcript
  //: search shards" about an export that plainly does, is worse than one that is a generation
  //: behind: the message is false and there is nothing for the reader to act on.
  //:
  //: `query.TimelineQuery._search_bootstrap` is the same fallback on the same file, including its
  //: refusal: two generations of one archive that disagree about `source_digest` describe two
  //: different builds, and answering phases out of one and messages out of the other would put
  //: messages inside phases they never occurred in. That is a refusal there and a refusal here.
  //: The rest of the binding is already in place and needs nothing new --
  //: `validateSchema2ObjectSourceDigest` checks every fetched object against `app.data`'s digest,
  //: which under schema 3 *is* the schema-3 bootstrap's.
  //:
  //: Returns the reason it could not be used, or "" when it was.
  async function schema3Schema2SearchFallback(bootstrap) {
    var older;
    try {
      older = await fetchJsonCached(SCHEMA_2_URL);
    } catch (error) {
      if (error && error.httpStatus === 404) {
        return SCHEMA_2_CORPUS_ABSENT;
      }
      return errorMessage(error);
    }
    if (!older || typeof older !== "object" || number(older.schema_version, NaN) !== 2 ||
        text(older.kind) !== "timeline-bootstrap") {
      return "the schema-2 bootstrap beside it is unreadable";
    }
    var ours = text(bootstrap.source_digest);
    var theirs = text(older.source_digest);
    if (ours && theirs && ours !== theirs) {
      return "the schema-2 corpus beside it belongs to a different source generation";
    }
    var corpus = schema2SearchCatalog(older);
    if (!corpus.catalog.length) {
      return SCHEMA_2_CORPUS_ABSENT;
    }
    app.searchCatalog = corpus.catalog;
    app.searchBloomByUrl = corpus.blooms;
    app.searchCorpusMode = "schema2";
    return "";
  }

  async function loadSchema2() {
    var bootstrap;
    try {
      bootstrap = await fetchJsonCached(SCHEMA_2_URL);
    } catch (error) {
      if (error && error.httpStatus === 404) {
        return false;
      }
      throw error;
    }
    if (!bootstrap || typeof bootstrap !== "object" ||
        number(bootstrap.schema_version, NaN) !== 2 ||
        text(bootstrap.kind) !== "timeline-bootstrap") {
      throw new Error("Unsupported schema-2 timeline bootstrap.");
    }
    var globalUrl = immutableTimelineObjectUrl(bootstrap.global, "timeline global");
    if (bootstrap.phase_index) {
      immutableTimelineObjectUrl(bootstrap.phase_index, "timeline phase index");
    }
    var globalData = await fetchContentAddressedJson(
      bootstrap.global,
      "timeline global"
    );
    if (!globalData || typeof globalData !== "object" ||
        number(globalData.schema_version, NaN) !== 2 ||
        text(globalData.kind) !== "timeline-global") {
      throw new Error("Unsupported schema-2 timeline global object.");
    }
    validateSchema2Generation(bootstrap, globalData);
    var catalog = array(bootstrap.detail_shards).slice();
    var seenUrls = new Set();
    catalog.forEach(function (shard) {
      var url = immutableTimelineObjectUrl(shard, "detail shard");
      var start = number(shard.start_ms, NaN);
      var end = number(shard.end_ms, NaN);
      if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start ||
          seenUrls.has(url)) {
        throw new Error("Invalid or duplicate schema-2 detail shard catalog entry.");
      }
      seenUrls.add(url);
    });
    catalog.sort(function (left, right) {
      return number(left.start_ms, 0) - number(right.start_ms, 0);
    });
    var schema2Corpus = schema2SearchCatalog(bootstrap);
    var searchCatalog = schema2Corpus.catalog;
    var searchBloomByUrl = schema2Corpus.blooms;
    app.schemaMode = "schema2";
    app.shardCatalog = catalog;
    app.searchCatalog = searchCatalog;
    app.searchBloomByUrl = searchBloomByUrl;
    app.detailPromises.clear();
    app.loadedShardUrls.clear();
    app.loadedSearchShardUrls.clear();
    app.loadedSearchLinkUrls.clear();
    app.searchRecords = [];
    app.searchRecordsByRef.clear();
    app.searchPromptExcerpts.clear();
    app.searchResponsesByPrompt.clear();
    app.searchLinkPromptRefs.clear();
    app.searchLinkResponseRefs.clear();
    app.searchShardState = catalog.length ? "unloaded" : "ready";
    app.searchCorpusMode = searchCatalog.length ? "schema2" : "none";
    app.searchCorpusNote = "";
    app.transcriptSearchState = searchCatalog.length ? "unloaded" : "unavailable";
    app.transcriptSearchError = searchCatalog.length
      ? ""
      : "This export does not contain transcript search shards.";
    app.phaseIndexReference = bootstrap.phase_index || null;
    app.phaseIndexPromise = null;
    app.phaseIndexByAgent.clear();
    resetSchema3State();
    var merged = Object.assign({}, globalData, {
      schema_version: 2,
      generated_at: bootstrap.generated_at,
      source_digest: bootstrap.source_digest,
      display_timezone: bootstrap.display_timezone,
      display_timezone_source: bootstrap.display_timezone_source,
      range: bootstrap.range,
      teams: bootstrap.teams,
      activity_bins: bootstrap.activity_bins,
      phases: [],
      events: []
    });
    initializeData(merged);
    return true;
  }

  async function loadSchema1() {
    app.schemaMode = "schema1";
    app.shardCatalog = [];
    app.searchCatalog = [];
    app.searchBloomByUrl.clear();
    app.detailPromises.clear();
    app.loadedShardUrls.clear();
    app.loadedSearchShardUrls.clear();
    app.loadedSearchLinkUrls.clear();
    app.searchRecords = [];
    app.searchRecordsByRef.clear();
    app.searchPromptExcerpts.clear();
    app.searchResponsesByPrompt.clear();
    app.searchLinkPromptRefs.clear();
    app.searchLinkResponseRefs.clear();
    app.searchShardState = "legacy";
    app.searchCorpusMode = "none";
    app.searchCorpusNote = "";
    app.transcriptSearchState = "unavailable";
    app.transcriptSearchError = "Transcript search requires a schema-2 export.";
    app.phaseIndexReference = null;
    app.phaseIndexPromise = null;
    app.phaseIndexByAgent.clear();
    resetSchema3State();
    initializeData(await fetchJsonCached(DATA_URL));
  }

  //: Newest generation first, each falling back to the one behind it.
  //:
  //: **Both directions of the mismatch are real, and neither is hypothetical.** A bundle is
  //: *copied* into the archive it was built with, so an archive built before schema 3 ships an
  //: `app.js` that has never heard of it -- which is exactly what `archive_gc._website_refusal`
  //: holds the schema-2 generation for. The converse happens whenever this bundle is opened
  //: against an older archive: a rebuild of one archive does not rebuild the others, and a
  //: reader that assumed the newest format would show an error on a tree that is perfectly
  //: readable. So both paths stay, and the fallback is announced in the meta line rather than
  //: swallowed, because "this archive is a generation behind" is a fact about the archive that
  //: only the page can see.
  async function loadTimeline() {
    try {
      var schema3Error = null;
      try {
        if (await loadSchema3()) {
          if (app.searchCorpusNote) {
            dom.meta.textContent += " · " + app.searchCorpusNote;
          }
          return;
        }
      } catch (error) {
        schema3Error = error;
      }
      var schema2Error = null;
      try {
        if (await loadSchema2()) {
          if (schema3Error) {
            dom.meta.textContent += " · schema 3 fallback (" +
              errorMessage(schema3Error) + ")";
            dom.card.dataset.timelineSchemaMode = "schema2-fallback";
          }
          return;
        }
      } catch (error) {
        schema2Error = error;
      }
      await loadSchema1();
      var reasons = [];
      if (schema3Error) {
        reasons.push("schema 3 fallback (" + errorMessage(schema3Error) + ")");
      }
      if (schema2Error) {
        reasons.push("schema 2 fallback (" + errorMessage(schema2Error) + ")");
      }
      if (reasons.length) {
        dom.meta.textContent += " · " + reasons.join(" · ");
        dom.card.dataset.timelineSchemaMode = "schema1-fallback";
      }
    } catch (error) {
      showLoadError(error);
    }
  }

  function errorMessage(error) {
    return error instanceof Error ? error.message : String(error);
  }

  function transcriptSearchNeedsLoad() {
    var missingText = transcriptSearchShards().some(function (shard) {
      return !app.loadedSearchShardUrls.has(shardKey(shard, "transcript search shard"));
    });
    if (missingText) {
      return true;
    }
    if (app.searchCorpusMode === "schema3") {
      // The relationship sidecar is one shard per team rather than one per day, so "have I got
      // the linkage" is asked of the teams in scope and not of every catalogue entry.
      return schema3SearchTeams().some(function (team) {
        var entry = app.searchLinksByTeam.get(team);
        return Boolean(entry) && !app.loadedSearchLinkUrls.has(entry.path);
      });
    }
    return app.searchCatalog.some(function (shard) {
      if (app.selectedTeam && text(shard.team) !== app.selectedTeam) {
        return false;
      }
      var reference = shard && shard.linkage;
      return reference && typeof reference === "object" &&
        !app.loadedSearchLinkUrls.has(
          immutableTimelineObjectUrl(reference, "transcript search linkage shard")
        );
    });
  }

  function refreshSearch() {
    if (!app.query) {
      updateTranscriptSearch();
      scheduleRender();
      return;
    }
    if (!transcriptSearchActive()) {
      app.transcriptSearchResults = [];
      app.transcriptSearchTotal = 0;
      app.transcriptMatchedAgentIds.clear();
      app.activeSearchRef = "";
      renderTranscriptSearchResults();
      if (shardedMode() && app.searchShardState !== "ready" &&
          app.searchShardState !== "loading") {
        requestSearchCorpus();
      }
      scheduleRender();
      return;
    }
    if (!shardedMode() || !app.searchCatalog.length) {
      app.transcriptSearchState = "unavailable";
      app.transcriptSearchError = shardedMode()
        ? "This export does not contain transcript search shards."
        : "Transcript search requires a sharded export.";
      updateShardDiagnostics();
      renderTranscriptSearchResults();
      scheduleRender();
      return;
    }
    if (transcriptSearchNeedsLoad() || app.transcriptSearchState !== "ready") {
      requestTranscriptSearchCorpus().catch(function () { return undefined; });
      return;
    }
    updateTranscriptSearch();
  }

  function invalidateTranscriptSearchRequest() {
    app.searchRequestGeneration += 1;
    app.detailRequest += 1;
    if (app.searchInputTimer !== null) {
      window.clearTimeout(app.searchInputTimer);
      app.searchInputTimer = null;
    }
  }

  function scheduleSearchRefresh() {
    invalidateTranscriptSearchRequest();
    if (!transcriptSearchActive() || !app.query) {
      refreshSearch();
      return;
    }
    app.transcriptSearchState = "loading";
    renderTranscriptSearchResults();
    app.searchInputTimer = window.setTimeout(function () {
      app.searchInputTimer = null;
      refreshSearch();
    }, SEARCH_INPUT_DEBOUNCE_MS);
  }

  function updateSearchPlaceholder() {
    dom.search.placeholder = {
      labels: "Agent, phase, message…",
      "owner-prompts": "Search my prompts…",
      "agent-responses": "Search agent responses…",
      "all-transcript": "Search all transcript messages…"
    }[app.searchScope] || "Search timeline…";
  }

  dom.teamFilter.addEventListener("change", function () {
    invalidateTranscriptSearchRequest();
    app.selectedTeam = dom.teamFilter.value;
    dom.scroll.scrollTop = 0;
    populateSummaryFiles();
    scheduleRender();
    refreshSearch();
  });

  dom.search.addEventListener("input", function () {
    app.query = dom.search.value.trim();
    dom.scroll.scrollTop = 0;
    app.activeSearchRef = "";
    scheduleSearchRefresh();
  });

  dom.searchScope.addEventListener("change", function () {
    invalidateTranscriptSearchRequest();
    app.searchScope = dom.searchScope.value;
    app.activeSearchRef = "";
    updateSearchPlaceholder();
    refreshSearch();
  });

  dom.searchSort.addEventListener("change", function () {
    app.searchSort = dom.searchSort.value;
    updateTranscriptSearch();
  });

  dom.searchResultsClose.addEventListener("click", function () {
    invalidateTranscriptSearchRequest();
    dom.search.value = "";
    app.query = "";
    app.activeSearchRef = "";
    updateTranscriptSearch();
    dom.search.focus();
  });

  updateSearchPlaceholder();

  dom.fit.addEventListener("click", fitTimeline);
  dom.zoomOut.addEventListener("click", function () {
    cancelRangeSelection();
    zoomAround(0.5, 1 / 0.72);
  });
  dom.zoomIn.addEventListener("click", function () {
    cancelRangeSelection();
    zoomAround(0.5, 0.72);
  });
  dom.glossaryOpen.addEventListener("click", function () {
    openGlossaryCatalog(true);
  });
  dom.perAgentTracks.addEventListener("change", function () {
    app.perAgentTracks = dom.perAgentTracks.checked;
    dom.scroll.scrollTop = 0;
    scheduleRender();
  });
  dom.showGlobalMessages.addEventListener("change", function () {
    app.showGlobalMessages = dom.showGlobalMessages.checked;
    scheduleRender();
  });
  dom.showHighlightedMessages.addEventListener("change", function () {
    app.showHighlightedMessages = dom.showHighlightedMessages.checked;
    scheduleRender();
  });
  dom.card.addEventListener("keydown", keyboardNavigate);
  dom.svg.addEventListener("wheel", wheelZoom, { passive: false });
  dom.axis.addEventListener("wheel", wheelZoom, { passive: false });
  dom.rollupTrack.addEventListener("wheel", wheelZoom, { passive: false });
  dom.svg.addEventListener("pointerdown", beginPan);
  dom.svg.addEventListener("pointermove", function (event) {
    updateRangeSelection(event);
    continuePan(event);
  });
  dom.svg.addEventListener("pointerup", endPan);
  dom.svg.addEventListener("pointercancel", endPan);
  dom.svg.addEventListener("click", handleEmptyTrackClick);
  dom.svg.addEventListener("contextmenu", cancelRangeSelectionOnContextMenu);
  dom.scroll.addEventListener("scroll", scheduleRender, { passive: true });
  dom.scroll.addEventListener("keydown", keyboardScrollTracks);
  window.addEventListener("hashchange", openGlossaryFromHash);

  dom.modalClose.addEventListener("click", closeModal);
  dom.modalBackdrop.addEventListener("click", function (event) {
    if (event.target === dom.modalBackdrop) {
      closeModal();
    }
  });
  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") {
      if (app.rangeSelection) {
        cancelRangeSelection();
      } else if (!dom.laneMenu.hidden) {
        hideLaneAgentMenu(true);
      } else if (!dom.contextMenu.hidden) {
        hideContextMenu();
      } else if (!dom.modalBackdrop.hidden) {
        closeModal();
      } else if (app.selection) {
        setSelection(null);
      }
      dom.summaryMenu.open = false;
    }
    if (!dom.modalBackdrop.hidden && event.key === "Tab") {
      var focusable = Array.from(dom.modalBackdrop.querySelectorAll(
        "button:not([disabled]), a[href], input:not([disabled]), select:not([disabled]), [tabindex]:not([tabindex='-1'])"
      ));
      if (!focusable.length) {
        return;
      }
      var first = focusable[0];
      var last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      }
    }
  });
  document.addEventListener("pointerdown", function (event) {
    if (!dom.contextMenu.hidden && !dom.contextMenu.contains(event.target)) {
      hideContextMenu();
    }
    if (!dom.laneMenu.hidden && !dom.laneMenu.contains(event.target) &&
        event.target !== app.laneMenuAnchor &&
        !(app.laneMenuAnchor && app.laneMenuAnchor.contains(event.target))) {
      hideLaneAgentMenu(false);
    }
  });
  window.addEventListener("blur", function () {
    hideContextMenu();
    hideLaneAgentMenu(false);
  });

  if (typeof ResizeObserver === "function") {
    new ResizeObserver(scheduleRender).observe(dom.card);
  } else {
    window.addEventListener("resize", scheduleRender, { passive: true });
  }

  loadTimeline();
}());
