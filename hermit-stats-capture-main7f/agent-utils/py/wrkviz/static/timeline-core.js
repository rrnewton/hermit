(function (root, factory) {
  "use strict";

  var api = factory();
  if (typeof module === "object" && module.exports) {
    module.exports = api;
  }
  root.AgentTimelineCore = api;
})(typeof globalThis === "object" ? globalThis : this, function () {
  "use strict";

  function finite(value, fallback) {
    return Number.isFinite(value) ? Number(value) : fallback;
  }

  function string(value) {
    return typeof value === "string" ? value : "";
  }

  /**
   * Select the amount of timeline detail that can remain legible at the current
   * time density. Keeping this calculation pure makes browser behavior and
   * performance budgets deterministic across viewport sizes.
   */
  function semanticZoomLevel(startMs, endMs, chartWidth) {
    var start = finite(startMs, 0);
    var end = finite(endMs, start + 1);
    var width = Math.max(1, finite(chartWidth, 1));
    var millisecondsPerPixel = Math.max(1, end - start) / width;
    if (millisecondsPerPixel <= 60 * 1000) {
      return "detail";
    }
    if (millisecondsPerPixel <= 5 * 60 * 1000) {
      return "lifetime";
    }
    return "aggregate";
  }

  /** Pick the narrowest precomputed bin that stays at least eight pixels wide. */
  function aggregateResolution(startMs, endMs, chartWidth) {
    var start = finite(startMs, 0);
    var end = finite(endMs, start + 1);
    var width = Math.max(1, finite(chartWidth, 1));
    var millisecondsPerPixel = Math.max(1, end - start) / width;
    if ((60 * 60 * 1000) / millisecondsPerPixel >= 8) {
      return "hourly";
    }
    if ((24 * 60 * 60 * 1000) / millisecondsPerPixel >= 8) {
      return "daily";
    }
    return "weekly";
  }

  /**
   * Calendar rollups can extend beyond the first/last recorded event. Include those
   * valid calendar bounds in the domain users may navigate without changing the
   * initially fitted event-data range.
   */
  function navigableRange(dataRange, rawRollups) {
    var source = dataRange && typeof dataRange === "object" ? dataRange : {};
    var start = finite(source.start_ms, 0);
    var end = finite(source.end_ms, start + 1);
    if (end <= start) {
      end = start + 1;
    }
    var supportedKinds = ["daily", "weekly", "monthly", "quarterly"];
    var rollups = Array.isArray(rawRollups) ? rawRollups : [];
    rollups.forEach(function (rollup) {
      if (!rollup || supportedKinds.indexOf(string(rollup.kind).toLowerCase()) < 0) {
        return;
      }
      var rollupStart = finite(rollup.start_ms, NaN);
      var rollupEnd = finite(rollup.end_ms, NaN);
      if (!Number.isFinite(rollupStart) || !Number.isFinite(rollupEnd) ||
          rollupEnd <= rollupStart) {
        return;
      }
      start = Math.min(start, rollupStart);
      end = Math.max(end, rollupEnd);
    });
    return { start_ms: start, end_ms: end };
  }

  /** Fit a requested view into a navigable domain while preserving its span. */
  function boundedViewRange(startMs, endMs, domain, minimumSpanMs) {
    var source = domain && typeof domain === "object" ? domain : {};
    var domainStart = finite(source.start_ms, 0);
    var domainEnd = finite(source.end_ms, domainStart + 1);
    if (domainEnd <= domainStart) {
      domainEnd = domainStart + 1;
    }
    var domainSpan = domainEnd - domainStart;
    var minimum = Math.min(
      domainSpan,
      Math.max(1, finite(minimumSpanMs, 1))
    );
    var requestedStart = finite(startMs, domainStart);
    var requestedEnd = finite(endMs, requestedStart + minimum);
    var span = Math.min(domainSpan, Math.max(minimum, requestedEnd - requestedStart));
    var start = Math.max(domainStart, requestedStart);
    if (start + span > domainEnd) {
      start = domainEnd - span;
    }
    return { start_ms: start, end_ms: start + span };
  }

  function minimumRangeWithin(startMs, endMs, boundsStart, boundsEnd, minimumSpanMs) {
    var boundsSpan = boundsEnd - boundsStart;
    var minimum = Math.min(boundsSpan, Math.max(1, finite(minimumSpanMs, 1)));
    var span = endMs - startMs;
    if (span >= minimum) {
      return { start_ms: startMs, end_ms: endMs };
    }
    var missing = minimum - Math.max(0, span);
    var start = Math.max(boundsStart, startMs - Math.floor(missing / 2));
    var end = start + minimum;
    if (end > boundsEnd) {
      end = boundsEnd;
      start = end - minimum;
    }
    return { start_ms: start, end_ms: end };
  }

  /**
   * Find meaningful work inside one half-open range, optionally scoped to one
   * agent or phase. Lifetimes and idle state only describe occupancy, not work.
   */
  function activityRangeWithin(bounds, data, minimumSpanMs, rawScope) {
    var boundsStart = finite(bounds && bounds.start_ms, NaN);
    var boundsEnd = finite(bounds && bounds.end_ms, NaN);
    if (!Number.isFinite(boundsStart) || !Number.isFinite(boundsEnd) ||
        boundsEnd <= boundsStart) {
      return null;
    }
    var source = data && typeof data === "object" ? data : {};
    var scope = rawScope && typeof rawScope === "object" ? rawScope : {};
    var agentId = string(scope.agent_id);
    var phaseId = string(scope.phase_id);
    var extentStart = Infinity;
    var extentEnd = -Infinity;

    function addInterval(startValue, endValue) {
      var start = finite(startValue, NaN);
      var end = finite(endValue, NaN);
      if (!Number.isFinite(start) || !Number.isFinite(end) || end <= start) {
        return false;
      }
      var clippedStart = Math.max(boundsStart, start);
      var clippedEnd = Math.min(boundsEnd, end);
      if (clippedEnd <= clippedStart) {
        return true;
      }
      extentStart = Math.min(extentStart, clippedStart);
      extentEnd = Math.max(extentEnd, clippedEnd);
      return true;
    }

    function addPoint(value) {
      var at = finite(value, NaN);
      if (!Number.isFinite(at) || at < boundsStart || at >= boundsEnd) {
        return;
      }
      extentStart = Math.min(extentStart, at);
      extentEnd = Math.max(extentEnd, Math.min(boundsEnd, at + 1));
    }

    (Array.isArray(source.phases) ? source.phases : []).forEach(function (phase) {
      if (!phase || typeof phase !== "object") {
        return;
      }
      if (phaseId && string(phase.id) !== phaseId) {
        return;
      }
      if (agentId && string(phase.agent_id) !== agentId) {
        return;
      }
      var states = Array.isArray(phase.states) ? phase.states : [];
      var hasValidState = false;
      states.forEach(function (state) {
        if (!state || typeof state !== "object") {
          return;
        }
        var stateStart = finite(state.start_ms, NaN);
        var stateEnd = finite(state.end_ms, NaN);
        if (!Number.isFinite(stateStart) || !Number.isFinite(stateEnd) ||
            stateEnd <= stateStart) {
          return;
        }
        hasValidState = true;
        if (string(state.kind).toLowerCase() !== "idle") {
          addInterval(stateStart, stateEnd);
        }
      });
      if (!hasValidState) {
        addInterval(phase.start_ms, phase.end_ms);
      }
    });

    (Array.isArray(source.events) ? source.events : []).forEach(function (event) {
      if (event && typeof event === "object") {
        if (agentId && string(event.agent_id) !== agentId) {
          return;
        }
        addPoint(event.at_ms);
      }
    });
    (Array.isArray(source.edges) ? source.edges : []).forEach(function (edge) {
      if (edge && typeof edge === "object") {
        if (!agentId || string(edge.source_id) === agentId) {
          addPoint(edge.source_ms);
        }
        if (!agentId || string(edge.target_id) === agentId) {
          addPoint(edge.target_ms);
        }
      }
    });

    if (!Number.isFinite(extentStart) || !Number.isFinite(extentEnd)) {
      var dataRange = source.range && typeof source.range === "object"
        ? source.range
        : {};
      var dataStart = finite(dataRange.start_ms, NaN);
      var dataEnd = finite(dataRange.end_ms, NaN);
      if (Number.isFinite(dataStart) && Number.isFinite(dataEnd)) {
        var fallbackStart = Math.max(boundsStart, dataStart);
        var fallbackEnd = Math.min(boundsEnd, dataEnd);
        if (fallbackEnd > fallbackStart) {
          extentStart = fallbackStart;
          extentEnd = fallbackEnd;
        }
      }
    }
    if (!Number.isFinite(extentStart) || !Number.isFinite(extentEnd)) {
      extentStart = boundsStart;
      extentEnd = boundsEnd;
    }
    return minimumRangeWithin(
      extentStart,
      extentEnd,
      boundsStart,
      boundsEnd,
      minimumSpanMs
    );
  }

  function compareLifetime(left, right) {
    return finite(left.start_ms, 0) - finite(right.start_ms, 0) ||
      finite(left.end_ms, finite(left.start_ms, 0)) -
        finite(right.end_ms, finite(right.start_ms, 0)) ||
      string(left.official_name).localeCompare(string(right.official_name), undefined, {
        numeric: true
      }) ||
      string(left.id).localeCompare(string(right.id), undefined, { numeric: true }) ||
      finite(left.input_index, 0) - finite(right.input_index, 0);
  }

  /**
   * Greedily pack half-open agent lifetimes into the first free lane.
   * Coordinators stay in dedicated leading lanes; subagents never share those lanes.
   */
  function packLifetimes(rawItems) {
    var items = Array.isArray(rawItems) ? rawItems.slice() : [];
    var laneById = Object.create(null);
    var dedicated = items.filter(function (item) { return Boolean(item.dedicated); });
    var packed = items.filter(function (item) { return !item.dedicated; });

    dedicated.forEach(function (item, index) {
      laneById[string(item.id)] = index;
    });

    var laneEnds = [];
    packed.sort(compareLifetime).forEach(function (item) {
      var start = finite(item.start_ms, 0);
      var end = Math.max(start, finite(item.end_ms, start));
      var lane = 0;
      while (lane < laneEnds.length && laneEnds[lane] > start) {
        lane += 1;
      }
      if (lane === laneEnds.length) {
        laneEnds.push(end);
      } else {
        laneEnds[lane] = end;
      }
      laneById[string(item.id)] = dedicated.length + lane;
    });

    return {
      lane_by_id: laneById,
      lane_count: dedicated.length + laneEnds.length
    };
  }

  /** Clip lifetimes to the visible half-open range and discard off-screen agents. */
  function lifetimesWithin(rawItems, startMs, endMs) {
    var viewStart = finite(startMs, 0);
    var viewEnd = finite(endMs, viewStart + 1);
    if (viewEnd <= viewStart) {
      return [];
    }
    return (Array.isArray(rawItems) ? rawItems : []).reduce(function (result, item) {
      var start = finite(item && item.start_ms, viewStart);
      var end = Math.max(start, finite(item && item.end_ms, start));
      if (end <= viewStart || start >= viewEnd) {
        return result;
      }
      result.push(Object.assign({}, item, {
        start_ms: Math.max(start, viewStart),
        end_ms: Math.min(end, viewEnd)
      }));
      return result;
    }, []);
  }

  /** First click selects an agent; repeated phase clicks toggle only that same phase. */
  function nextPhaseSelection(current, agentId, phaseId, startMs, endMs) {
    var agent = string(agentId);
    var phase = string(phaseId);
    if (current && current.kind === "agent" && current.agent_id === agent) {
      return {
        kind: "phase",
        agent_id: agent,
        phase_id: phase,
        start_ms: finite(startMs, 0),
        end_ms: finite(endMs, finite(startMs, 0))
      };
    }
    if (current && current.kind === "phase" &&
        current.agent_id === agent && current.phase_id === phase) {
      return { kind: "agent", agent_id: agent };
    }
    if (current && current.kind === "phase" && current.agent_id === agent) {
      return {
        kind: "phase",
        agent_id: agent,
        phase_id: phase,
        start_ms: finite(startMs, 0),
        end_ms: finite(endMs, finite(startMs, 0))
      };
    }
    return { kind: "agent", agent_id: agent };
  }

  function edgeTouchesSelection(edge, selection) {
    if (!selection) {
      return false;
    }
    if (selection.kind === "edge") {
      return string(edge.id) === string(selection.edge_id);
    }
    if (selection.kind === "rollup") {
      var rollupStart = finite(selection.start_ms, -Infinity);
      var rollupEnd = finite(selection.end_ms, Infinity);
      var sourceAt = finite(edge.source_ms, NaN);
      var targetAt = finite(edge.target_ms, NaN);
      return (Number.isFinite(sourceAt) && sourceAt >= rollupStart && sourceAt <= rollupEnd) ||
        (Number.isFinite(targetAt) && targetAt >= rollupStart && targetAt <= rollupEnd);
    }
    var agentId = string(selection.agent_id);
    var sourceMatches = string(edge.source_id) === agentId;
    var targetMatches = string(edge.target_id) === agentId;
    if (!sourceMatches && !targetMatches) {
      return false;
    }
    if (selection.kind !== "phase") {
      return true;
    }
    var start = finite(selection.start_ms, -Infinity);
    var end = finite(selection.end_ms, Infinity);
    var agentAt = sourceMatches
      ? finite(edge.source_ms, NaN)
      : finite(edge.target_ms, NaN);
    return Number.isFinite(agentAt) && agentAt >= start && agentAt <= end;
  }

  /** Return hidden/normal/dimmed/highlighted for one structural or detailed edge. */
  function edgeDisplayState(edge, selection, showGlobalDetailed, showHighlightedDetailed) {
    var kind = string(edge.kind).toLowerCase();
    // Spawns, explicit session continuations, and lifetime results are structural. Keep
    // them visible as context; only intermediate message traffic obeys the detailed-edge
    // toggles.
    var structural = kind === "spawn" || kind === "continuation" || kind === "result";
    var highlighted = edgeTouchesSelection(edge, selection);
    if (structural) {
      if (!selection) {
        return "normal";
      }
      return highlighted ? "highlighted" : "dimmed";
    }
    if (highlighted && showHighlightedDetailed) {
      return "highlighted";
    }
    if (showGlobalDetailed) {
      return selection ? "dimmed" : "normal";
    }
    return "hidden";
  }

  return {
    aggregateResolution: aggregateResolution,
    activityRangeWithin: activityRangeWithin,
    boundedViewRange: boundedViewRange,
    edgeDisplayState: edgeDisplayState,
    edgeTouchesSelection: edgeTouchesSelection,
    lifetimesWithin: lifetimesWithin,
    navigableRange: navigableRange,
    nextPhaseSelection: nextPhaseSelection,
    packLifetimes: packLifetimes,
    semanticZoomLevel: semanticZoomLevel
  };
});
