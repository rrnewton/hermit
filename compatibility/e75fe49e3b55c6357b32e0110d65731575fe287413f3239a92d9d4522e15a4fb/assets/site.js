"use strict";

(function (root) {
  const HERMIT_COMMIT_BASE = "https://github.com/rrnewton/hermit/commit/";
  const QUERY_ORDER = [
    'q',
    'lane',
    'category',
    'mode',
    'backend',
    'state',
    'result',
    'selected_by_path',
    'current_main_ancestry',
    'host_capability',
    'host_capability_present',
    'evidence',
    'population',
    'sort',
    'direction',
    'page',
  ];
  const DIRECT_FILTERS = [
    'lane',
    'category',
    'mode',
    'backend',
    'state',
    'result',
    'evidence',
  ];
  const CURRENT_ONLY = [
    ...DIRECT_FILTERS,
    "selected_by_path",
    "current_main_ancestry",
    "host_capability",
    "host_capability_present",
  ];
  const ENUMS = {
    state: ["green", "red", "never-measured", "unavailable"],
    result: ["pass", "product_failure", "unavailable"],
    current_main_ancestry: ["true", "false", "unavailable"],
    host_capability_present: ["true", "false", "unavailable"],
    evidence: ["complete", "incomplete"],
    population: ["in_manifest", "not_in_current_manifest"],
    sort: [
      "manifest",
      "cell",
      "test",
      "lane",
      "category",
      "mode",
      "backend",
      "selected_by_path",
      "state",
      "result",
      "latest_result_time",
      "latest_result_tree",
      "physical_row_count",
      "represented_run_count",
    ],
    direction: ["asc", "desc"],
  };
  const OUTSIDE_SORTS = new Set(["cell", "physical_row_count", "represented_run_count"]);
  const NUMERIC_SORTS = new Set(["manifest", "physical_row_count", "represented_run_count"]);
  const STATE_ORDER = new Map([
    ["green", 0],
    ["red", 1],
    ["never-measured", 2],
    ["unavailable", 3],
  ]);

  function defaults(population) {
    const value = population || "in_manifest";
    return {
      q: "",
      lane: "",
      category: "",
      mode: "",
      backend: "",
      state: "",
      result: "",
      selected_by_path: "",
      current_main_ancestry: "",
      host_capability: "",
      host_capability_present: "",
      evidence: "",
      population: value,
      sort: value === 'in_manifest' ? 'manifest' : 'cell',
      direction: 'asc',
      page: 1,
    };
  }

  function invalid(message) {
    return { ok: false, error: message };
  }

  function allowedValue(name, value, allowed) {
    const fixed = ENUMS[name];
    if (fixed) {
      return fixed.includes(value);
    }
    const values = allowed[name];
    return Array.isArray(values) && values.includes(value);
  }

  function parseQuery(search, allowed) {
    const raw = search.startsWith("?") ? search.slice(1) : search;
    if (/%(?![0-9a-f]{2})/i.test(raw)) {
      return invalid("the URL contains an invalid percent escape");
    }
    const params = new URLSearchParams(raw);
    const supplied = {};
    for (const [name, value] of params) {
      if (!QUERY_ORDER.includes(name)) {
        return invalid(`the URL contains unsupported filter ${name}`);
      }
      if (Object.prototype.hasOwnProperty.call(supplied, name)) {
        return invalid(`the URL repeats filter ${name}`);
      }
      supplied[name] = value;
    }
    const population = supplied.population || "in_manifest";
    if (!allowedValue("population", population, allowed)) {
      return invalid(`population has unsupported value ${population}`);
    }
    const state = defaults(population);
    for (const name of QUERY_ORDER) {
      if (!Object.prototype.hasOwnProperty.call(supplied, name)) {
        continue;
      }
      const value = supplied[name];
      if (name === "q") {
        const trimmed = value.trim();
        if (!trimmed || trimmed.length > 256) {
          return invalid("q must contain between 1 and 256 non-space characters");
        }
        state.q = trimmed;
        continue;
      }
      if (name === 'page') {
        if (!/^[1-9][0-9]*$/.test(value)) {
          return invalid('page must be a positive integer');
        }
        const page = Number(value);
        if (!Number.isSafeInteger(page)) {
          return invalid('page is too large');
        }
        state.page = page;
        continue;
      }
      if (!value || !allowedValue(name, value, allowed)) {
        return invalid(`${name} has unsupported value ${value || "(empty)"}`);
      }
      state[name] = value;
    }
    const capability = Boolean(state.host_capability);
    const capabilityValue = Boolean(state.host_capability_present);
    if (capability !== capabilityValue) {
      return invalid("host_capability and host_capability_present must be supplied together");
    }
    if (population === "not_in_current_manifest") {
      const unsupported = CURRENT_ONLY.find((name) => state[name]);
      if (unsupported) {
        return invalid(`${unsupported} is unavailable outside the current manifest`);
      }
      if (!OUTSIDE_SORTS.has(state.sort)) {
        return invalid(`${state.sort} cannot sort identities outside the current manifest`);
      }
    }
    return { ok: true, state };
  }

  function serializeState(state) {
    const params = new URLSearchParams();
    const baseline = defaults(state.population);
    for (const name of QUERY_ORDER) {
      const value = state[name];
      if (name === "population") {
        if (value !== "in_manifest") {
          params.append(name, value);
        }
        continue;
      }
      if (value && value !== baseline[name]) {
        params.append(name, String(value));
      }
    }
    return params.toString();
  }

  function unavailableRowEvidenceMatches(state) {
    if (state.current_main_ancestry && state.current_main_ancestry !== "unavailable") {
      return false;
    }
    if (state.host_capability && state.host_capability_present !== "unavailable") {
      return false;
    }
    return true;
  }

  function oneEvidenceRowMatches(evidence, state) {
    if (
      state.current_main_ancestry &&
      evidence.current_main_ancestry !== state.current_main_ancestry
    ) {
      return false;
    }
    if (state.host_capability) {
      const capabilities = evidence.host_capabilities || {};
      const value = Object.prototype.hasOwnProperty.call(capabilities, state.host_capability)
        ? capabilities[state.host_capability]
        : "unavailable";
      if (value !== state.host_capability_present) {
        return false;
      }
    }
    return true;
  }

  function rowScopedEvidenceMatches(record, state) {
    if (!state.current_main_ancestry && !state.host_capability) {
      return true;
    }
    if (record.series_filter_facets.length === 0) {
      return unavailableRowEvidenceMatches(state);
    }
    return record.series_filter_facets.some((evidence) =>
      oneEvidenceRowMatches(evidence, state),
    );
  }

  function matches(record, state) {
    if (record.population !== state.population) {
      return false;
    }
    if (state.q && !record.search.toLowerCase().includes(state.q.toLowerCase())) {
      return false;
    }
    if (state.population === "not_in_current_manifest") {
      return true;
    }
    for (const name of DIRECT_FILTERS) {
      if (state[name] && record[name] !== state[name]) {
        return false;
      }
    }
    if (state.selected_by_path && !record.selected_by_paths.includes(state.selected_by_path)) {
      return false;
    }
    return rowScopedEvidenceMatches(record, state);
  }

  function stringCompare(left, right) {
    if (left === right) {
      return 0;
    }
    return left < right ? -1 : 1;
  }

  function comparePrimary(left, right, sort) {
    if (sort === "state") {
      return STATE_ORDER.get(left[sort]) - STATE_ORDER.get(right[sort]);
    }
    if (NUMERIC_SORTS.has(sort)) {
      return Number(left[sort]) - Number(right[sort]);
    }
    const comparable = value => Array.isArray(value) ? value.join(', ') : value;
    const leftValue = comparable(left[sort]) || "";
    const rightValue = comparable(right[sort]) || "";
    if (!leftValue || !rightValue) {
      if (!leftValue && !rightValue) {
        return 0;
      }
      return leftValue ? -1 : 1;
    }
    return stringCompare(leftValue, rightValue);
  }

  function sortRecords(records, state) {
    return records.slice().sort((left, right) => {
      const leftUnavailable = !NUMERIC_SORTS.has(state.sort) && !left[state.sort];
      const rightUnavailable = !NUMERIC_SORTS.has(state.sort) && !right[state.sort];
      if (leftUnavailable !== rightUnavailable) {
        return leftUnavailable ? 1 : -1;
      }
      const primary = comparePrimary(left, right, state.sort);
      if (primary !== 0) {
        return state.direction === "desc" ? -primary : primary;
      }
      return stringCompare(left.cell, right.cell);
    });
  }

  function summarize(records) {
    return records.reduce(
      (summary, record) => ({
        matching: summary.matching + 1,
        physical: summary.physical + record.physical_row_count,
        represented: summary.represented + record.represented_run_count,
      }),
      { matching: 0, physical: 0, represented: 0 },
    );
  }

  function pageRecords(records, page, pageSize) {
    if (!Number.isSafeInteger(page) || page < 1) {
      return invalid('page must be a positive integer');
    }
    if (!Number.isSafeInteger(pageSize) || pageSize < 1) {
      return invalid('page size must be a positive integer');
    }
    const pages = Math.max(1, Math.ceil(records.length / pageSize));
    if (page > pages) {
      return invalid(`page ${page} exceeds the ${pages} available pages`);
    }
    const start = (page - 1) * pageSize;
    return {
      ok: true,
      records: records.slice(start, start + pageSize),
      start,
      pages,
    };
  }

  const api = Object.freeze({
    matches,
    pageRecords,
    parseQuery,
    serializeState,
    sortRecords,
    summarize,
  });
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
  root.HermitCompatibilityCellListing = api;

  function manifestRecord(cell, index) {
    return {
      source: cell,
      population: "in_manifest",
      manifest: index,
      cell: cell.cell,
      test: cell.test,
      lane: cell.lane,
      category: cell.category,
      mode: cell.mode,
      backend: cell.backend,
      state: derivedCellState(cell).toLowerCase().replace(' ', '-'),
      result: cell.latest_result || '',
      selected_by_paths: cell.selected_by_paths,
      evidence: 'complete',
      latest_result_time: cell.latest_result_time || '',
      latest_result_tree: cell.latest_result_tree || '',
      search: [cell.cell, cell.test, cell.lane, cell.category, cell.mode, cell.backend].join(' '),
      series_filter_facets: cell.series_filter_facets,
      physical_row_count: cell.physical_row_count,
      represented_run_count: cell.represented_run_count,
    };
  }

  function outsideRecord(item, index) {
    return {
      source: item,
      population: "not_in_current_manifest",
      manifest: index,
      cell: item.cell,
      search: item.cell,
      series_filter_facets: [],
      physical_row_count: item.physical_row_count,
      represented_run_count: item.represented_run_count,
    };
  }

  function selectValues(form, name) {
    const control = form.elements.namedItem(name);
    return Array.from(control.options)
      .map((option) => option.value)
      .filter(Boolean);
  }

  function allowedValues(form) {
    return {
      lane: selectValues(form, "lane"),
      category: selectValues(form, "category"),
      mode: selectValues(form, "mode"),
      backend: selectValues(form, "backend"),
      selected_by_path: selectValues(form, "selected_by_path"),
      host_capability: selectValues(form, "host_capability"),
    };
  }

  function setControls(form, state) {
    for (const name of QUERY_ORDER) {
      const control = form.elements.namedItem(name);
      if (control) {
        control.value = state[name];
      }
    }
  }

  function applyDependentCellFilterTransaction(view, state) {
    const names = [
      'lane',
      'category',
      'mode',
      'backend',
      'state',
      'result',
      'selected_by_path',
    ];
    const plans = [];
    for (const name of names) {
      const control = view.form.elements.namedItem(name);
      if (!control || control.type === 'hidden') {
        continue;
      }
      plans.push({name, control, available: new Set(), selected: state[name]});
    }
    const baseline = {...state};
    for (const plan of plans) {
      baseline[plan.name] = '';
    }
    const active = plans.filter(plan => plan.selected);
    let visitedRecords = 0;
    for (const record of view.allRecords) {
      visitedRecords += 1;
      if (!matches(record, baseline)) {
        continue;
      }
      let mismatch = null;
      let multipleMismatches = false;
      for (const plan of active) {
        const matchesSelection =
          plan.name === 'selected_by_path'
            ? record.selected_by_paths.includes(plan.selected)
            : record[plan.name] === plan.selected;
        if (!matchesSelection) {
          if (mismatch !== null) {
            multipleMismatches = true;
            break;
          }
          mismatch = plan.name;
        }
      }
      if (multipleMismatches) {
        continue;
      }
      for (const plan of plans) {
        if (mismatch !== null && mismatch !== plan.name) {
          continue;
        }
        const values =
          plan.name === 'selected_by_path'
            ? record.selected_by_paths
            : [record[plan.name]];
        for (const value of values) {
          if (value) {
            plan.available.add(String(value));
          }
        }
      }
    }
    for (const plan of plans) {
      for (const option of plan.control.options) {
        option.disabled = Boolean(
          option.value &&
          option.value !== plan.selected &&
          !plan.available.has(option.value)
        );
      }
    }
    return visitedRecords;
  }

  function stateFromControls(form) {
    const state = {};
    for (const name of QUERY_ORDER) {
      const control = form.elements.namedItem(name);
      state[name] = control ? control.value.trim() : "";
    }
    return state;
  }

  function setCurrentManifestControls(form, population) {
    const outside = population === "not_in_current_manifest";
    for (const name of CURRENT_ONLY) {
      const control = form.elements.namedItem(name);
      control.disabled = outside;
      if (outside) {
        control.value = "";
      }
    }
    const sort = form.elements.namedItem("sort");
    for (const option of sort.options) {
      option.disabled = outside && !OUTSIDE_SORTS.has(option.value);
    }
    if (outside && !OUTSIDE_SORTS.has(sort.value)) {
      sort.value = "cell";
    }
    if (!outside) {
      const capability = form.elements.namedItem("host_capability");
      form.elements.namedItem("host_capability_present").disabled = !capability.value;
    }
  }

  function commitUrl(url, push) {
    if (url.href === root.location.href) {
      return false;
    }
    if (push && historyPushes < 24) {
      root.history.pushState(null, '', url.href);
      historyPushes += 1;
    } else {
      root.history.replaceState(null, '', url.href);
    }
    return true;
  }

  function updateUrl(state, push) {
    const url = new URL(root.location.href);
    url.search = serializeState(state);
    url.hash = state.population === "in_manifest" ? "all-cells" : "recorded-not-in-current-manifest";
    commitUrl(url, push);
  }

  function browserView(form, summary, urls, pageKind) {
    const currentBody = document.getElementById('cell-results');
    const wantZero = pageKind === 'never-list';
    const currentRows = summary.cells
      .filter(cell => (cell.qualifying_physical_row_count === 0) === wantZero)
      .map(manifestRecord);
    const outsideRows = summary.recorded_outside_manifest.map(outsideRecord);
    const sortButtons = Array.from(document.querySelectorAll('[data-cell-sort]'));
    if (sortButtons.some(button => !ENUMS.sort.includes(button.dataset.cellSort))) {
      throw new Error('cell table contains an unsupported sort heading');
    }
    return {
      form,
      currentRows,
      outsideRows,
      allRecords: currentRows.concat(outsideRows),
      currentBody,
      outsideBody: document.getElementById(
        'recorded-not-in-current-manifest-results',
      ),
      currentSections: Array.from(
        document.querySelectorAll('[data-in-manifest-listing]'),
      ),
      outsideSection: document.getElementById(
        'recorded-not-in-current-manifest',
      ),
      error: document.getElementById('cell-filter-error'),
      empty: document.getElementById('cell-filter-empty'),
      matchingCount: document.querySelector('[data-matching-count]'),
      counts: document.getElementById('cell-filter-counts'),
      physicalCount: document.querySelector('[data-physical-count]'),
      representedCount: document.querySelector('[data-represented-count]'),
      matchingLabel: document.querySelector('[data-matching-label]'),
      evidenceLabel: document.querySelector('[data-evidence-label]'),
      pagination: document.querySelector('[data-cell-pagination]'),
      pageSummary: document.querySelector('[data-cell-page-summary]'),
      previous: document.querySelector('[data-cell-previous]'),
      next: document.querySelector('[data-cell-next]'),
      sortButtons,
      allowed: allowedValues(form),
      tests: new Map(summary.tests.map(test => [test.id, test])),
      urls,
      state: null,
      searchEditing: false,
    };
  }

  function updateCellSortButtons(view, state) {
    for (const button of view.sortButtons) {
      const active = button.dataset.cellSort === state.sort;
      const heading = button.closest('th');
      const indicator = button.querySelector('.sort-indicator');
      if (!heading || !indicator) {
        throw new Error('cell sort heading is incomplete');
      }
      heading.setAttribute(
        'aria-sort',
        active ? (state.direction === 'asc' ? 'ascending' : 'descending') : 'none',
      );
      indicator.textContent = active ? (state.direction === 'asc' ? '▲' : '▼') : '';
    }
  }

  function hydratedOutsideRow(record) {
    const row = dataNode('tr');
    row.dataset.recordedNotInManifestRow = '1';
    row.dataset.cell = record.cell;
    row.dataset.search = record.search;
    row.dataset.physicalRowCount = String(record.physical_row_count);
    row.dataset.representedRunCount = String(record.represented_run_count);
    const identity = dataNode('code', record.cell);
    appendTableCell(row, identity, true);
    appendTableCell(row, record.physical_row_count, false);
    appendTableCell(row, record.represented_run_count, false);
    return row;
  }

  function renderCellPage(view, body, records) {
    const rows = records.map(record =>
      record.population === 'in_manifest'
        ? hydratedCellRow(record.source, view.tests, view.urls)
        : hydratedOutsideRow(record),
    );
    body.replaceChildren(...rows);
  }

  function showInvalid(view, message) {
    view.form.reset();
    setCurrentManifestControls(view.form, "in_manifest");
    view.error.textContent = `Filters were not applied: ${message}.`;
    view.error.hidden = false;
    view.empty.hidden = true;
    view.counts.hidden = true;
    view.pagination.hidden = true;
    const initial = sortRecords(view.currentRows, defaults('in_manifest')).slice(
      0,
      Number(view.pagination.dataset.pageSize),
    );
    renderCellPage(view, view.currentBody, initial);
    view.outsideBody.replaceChildren();
    for (const section of view.currentSections) {
      section.hidden = false;
    }
    view.outsideSection.hidden = true;
    view.state = defaults('in_manifest');
    updateCellSortButtons(view, view.state);
  }

  function showFormError(view, message) {
    view.error.textContent = `Filters were not applied: ${message}.`;
    view.error.hidden = false;
  }

  function applyView(view, state, historyAction) {
    const populationRecords = view.allRecords.filter(
      (record) => record.population === state.population,
    );
    const matching = sortRecords(
      populationRecords.filter((record) => matches(record, state)),
      state,
    );
    const page = pageRecords(
      matching,
      state.page,
      Number(view.pagination.dataset.pageSize),
    );
    if (!page.ok) {
      showInvalid(view, page.error);
      return false;
    }
    const body = state.population === "in_manifest" ? view.currentBody : view.outsideBody;
    const otherBody = state.population === "in_manifest" ? view.outsideBody : view.currentBody;
    renderCellPage(view, body, page.records);
    otherBody.replaceChildren();
    const current = state.population === "in_manifest";
    for (const section of view.currentSections) {
      section.hidden = !current;
    }
    view.outsideSection.hidden = current;
    const summary = summarize(matching);
    view.matchingCount.textContent = String(summary.matching);
    view.physicalCount.textContent = String(summary.physical);
    view.representedCount.textContent = String(summary.represented);
    view.matchingLabel.textContent = current
      ? "matching cells in the manifest"
      : "matching recorded cell identities not in the current manifest";
    view.evidenceLabel.textContent = current ? "those cells have" : "those identities have";
    view.empty.hidden = summary.matching !== 0;
    view.counts.hidden = false;
    const first = summary.matching === 0 ? 0 : page.start + 1;
    const last = page.start + page.records.length;
    view.pageSummary.textContent = `Showing ${first}–${last} of ${summary.matching} matching cells; page ${state.page} of ${page.pages}.`;
    view.previous.disabled = state.page === 1;
    view.next.disabled = state.page === page.pages;
    view.pagination.hidden = page.pages <= 1;
    view.error.hidden = true;
    const visitedRecords = applyDependentCellFilterTransaction(view, state);
    if (visitedRecords !== view.allRecords.length) {
      throw new Error('dependent cell filters did not visit every source record');
    }
    view.form.dataset.dependentFilterVisitedRecords = String(visitedRecords);
    view.state = state;
    updateCellSortButtons(view, state);
    if (historyAction) {
      updateUrl(state, historyAction === 'push');
    }
    return true;
  }

  function applyControls(view, resetPage, historyAction) {
    if (resetPage) {
      view.form.elements.namedItem('page').value = '1';
    }
    const state = stateFromControls(view.form);
    const result = parseQuery(`?${serializeState(state)}`, view.allowed);
    if (!result.ok) {
      showFormError(view, result.error);
      return false;
    }
    if (
      view.state !== null &&
      serializeState(view.state) === serializeState(result.state)
    ) {
      setControls(view.form, result.state);
      return true;
    }
    return applyView(view, result.state, historyAction);
  }

  function applyLocation(view) {
    const result = parseQuery(root.location.search, view.allowed);
    if (!result.ok) {
      showInvalid(view, result.error);
      return;
    }
    setControls(view.form, result.state);
    setCurrentManifestControls(view.form, result.state.population);
    applyView(view, result.state, false);
  }

  function bind(view) {
    const form = view.form;
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      applyControls(view, true, 'push');
      view.searchEditing = false;
    });
    form.addEventListener('input', event => {
      if (event.target.name === 'q') {
        const action = view.searchEditing ? 'replace' : 'push';
        if (applyControls(view, true, action)) {
          view.searchEditing = true;
        }
      }
    });
    form.addEventListener("change", (event) => {
      if (event.target.name === "population") {
        setCurrentManifestControls(form, event.target.value);
      } else if (event.target.name === "host_capability") {
        const present = form.elements.namedItem("host_capability_present");
        present.disabled = !event.target.value;
        if (!event.target.value) {
          present.value = "";
        } else if (!present.value) {
          return;
        }
      } else if (
        event.target.name === 'host_capability_present' &&
        !form.elements.namedItem('host_capability').value
      ) {
        return;
      }
      if (applyControls(view, true, 'push')) {
        view.searchEditing = false;
      }
    });
    form
      .querySelector('[data-clear-cell-filters]')
      .addEventListener('click', () => {
        form.reset();
        setCurrentManifestControls(form, 'in_manifest');
        applyView(view, defaults('in_manifest'), 'push');
        view.searchEditing = false;
      });
    view.previous.addEventListener('click', event => {
      event.currentTarget.blur();
      const page = form.elements.namedItem('page');
      page.value = String(Math.max(1, Number(page.value) - 1));
      applyControls(view, false, 'push');
      view.searchEditing = false;
    });
    view.next.addEventListener('click', event => {
      event.currentTarget.blur();
      const page = form.elements.namedItem('page');
      page.value = String(Number(page.value) + 1);
      applyControls(view, false, 'push');
      view.searchEditing = false;
    });
    for (const button of view.sortButtons) {
      button.addEventListener('click', event => {
        const sort = event.currentTarget.dataset.cellSort;
        const direction =
          view.state.sort === sort && view.state.direction === 'asc' ? 'desc' : 'asc';
        form.elements.namedItem('sort').value = sort;
        form.elements.namedItem('direction').value = direction;
        applyControls(view, true, 'push');
        view.searchEditing = false;
      });
    }
    root.addEventListener('popstate', () => {
      view.searchEditing = false;
      applyLocation(view);
    });
  }

  function genericRecord(element, ordinal) {
    if (element.dataset.listItem !== '1') {
      throw new Error('rendered collection item has an invalid marker');
    }
    const values = {};
    for (const attribute of element.attributes) {
      if (!attribute.name.startsWith('data-list-value-')) {
        continue;
      }
      const name = attribute.name
        .slice('data-list-value-'.length)
        .replaceAll('-', '_');
      values[name] = attribute.value;
    }
    return {
      element,
      ordinal,
      search: `${element.textContent || ''} ${Object.values(values).join(' ')}`,
      values,
      groupId: '',
      parent: element.parentElement,
    };
  }

  function inlineRecordedCellRecord(value, ordinal, parent) {
    exactObject(
      value,
      ['cell', 'details', 'href', 'test_result', 'tier', 'verdict'],
      'recorded cell',
    );
    for (const name of ['cell', 'test_result', 'tier', 'verdict']) {
      if (typeof value[name] !== 'string' || !value[name]) {
        throw new Error(`recorded cell ${name} is invalid`);
      }
    }
    if (
      (value.href !== null && (typeof value.href !== 'string' || !value.href)) ||
      !Array.isArray(value.details) ||
      value.details.some(detail => typeof detail !== 'string')
    ) {
      throw new Error('recorded cell details or link are invalid');
    }
    const details = value.details.join(' ');
    return {
      element: null,
      ordinal,
      search: `${value.cell} ${value.test_result} ${value.verdict} ${value.tier} ${details}`,
      values: {
        cell: value.cell,
        details,
        test_result: value.test_result,
        tier: value.tier,
        verdict: value.verdict,
      },
      groupId: '',
      parent,
      source: value,
    };
  }

  function recordedCellElement(record) {
    const value = record.source;
    const row = dataNode('tr');
    row.dataset.recordedCellRow = '1';
    row.dataset.listItem = '1';
    for (const [name, item] of Object.entries(record.values)) {
      row.setAttribute(`data-list-value-${name.replaceAll('_', '-')}`, item);
    }
    const label = dataNode('code', value.cell);
    if (value.href === null) {
      appendTableCell(row, label, true);
    } else {
      const url = new URL(value.href, root.location.href);
      const parameters = Array.from(url.searchParams.entries());
      if (
        url.origin !== root.location.origin ||
        !url.pathname.endsWith('/cells/detail.html') ||
        url.hash ||
        parameters.length !== 1 ||
        parameters[0][0] !== 'cell' ||
        !DETAIL_PATTERN.test(parameters[0][1])
      ) {
        throw new Error('recorded cell link is invalid');
      }
      const link = dataNode('a');
      link.href = url.href;
      link.appendChild(label);
      appendTableCell(row, link, true);
    }
    appendTableCell(row, value.test_result, false);
    appendTableCell(row, value.verdict, false);
    appendTableCell(row, value.tier, false);
    const detail = document.createDocumentFragment();
    value.details.forEach((text, index) => {
      if (index) {
        detail.appendChild(document.createElement('br'));
      }
      detail.appendChild(document.createTextNode(text));
    });
    appendTableCell(row, detail, false);
    return row;
  }

  function inlineCollectionRecords(scope, collectionId, pageSize) {
    const template = scope.querySelector(
      `template[data-list-records="${collectionId}"]`,
    );
    if (!template) {
      return null;
    }
    const target = scope.querySelector(`[data-list-target="${collectionId}"]`);
    const source = template.content.querySelector('[data-list-records-json]');
    if (!target || !source) {
      throw new Error(`rendered collection ${collectionId} has invalid inline data`);
    }
    let values;
    try {
      values = JSON.parse(source.textContent || '');
    } catch (_error) {
      throw new Error(`rendered collection ${collectionId} inline data is invalid JSON`);
    }
    if (!Array.isArray(values)) {
      throw new Error(`rendered collection ${collectionId} inline data is not a list`);
    }
    if (collectionId !== 'recorded-cells') {
      throw new Error(`rendered collection ${collectionId} has no inline renderer`);
    }
    const records = values.map((value, index) =>
      inlineRecordedCellRecord(value, index, target),
    );
    const initial = target.querySelectorAll('[data-list-item]');
    if (initial.length !== Math.min(records.length, pageSize)) {
      throw new Error(`rendered collection ${collectionId} initial page is incomplete`);
    }
    return {records, render: recordedCellElement, target};
  }

  function groupedRecord(element, ordinal) {
    const mode = element.dataset.mode || '';
    const backend = element.dataset.backend || '';
    const category = element.dataset.category || '';
    return {
      element,
      ordinal,
      search: element.textContent || '',
      values: {
        mode,
        backend,
        category,
        hierarchy: `${mode}/${backend}/${category}`,
        test:
          element.querySelector(
            ':scope > a, :scope > .selected-cell-without-detail',
          )?.textContent || '',
      },
      groupId: element.dataset.listGroup || '',
      parent: element.parentElement,
    };
  }

  function groupedChildRecord(element, ordinal, group) {
    const test = element.textContent || '';
    return {
      element,
      ordinal,
      search: `${test} ${group.values.mode} ${group.values.backend} ${group.values.category}`,
      values: {
        mode: group.values.mode,
        backend: group.values.backend,
        category: group.values.category,
        hierarchy: `${group.values.hierarchy}/${test}`,
        test,
      },
      groupId: group.groupId,
      parent: element.parentElement,
    };
  }

  function genericFilterNames(form) {
    return Array.from(
      form.querySelectorAll('[data-list-filter]'),
      control => control.dataset.listFilter,
    );
  }

  function genericDefaults(view) {
    return {
      q: '',
      filters: Object.fromEntries(view.filterNames.map(name => [name, ''])),
      sort: view.defaultSort,
      direction: view.defaultDirection,
      page: 1,
      expanded: new Set(),
    };
  }

  function genericQueryName(view, name) {
    return `${view.queryPrefix}${name}`;
  }

  function genericQueryFields(view) {
    const fields = ['q', ...view.filterNames, 'sort', 'direction', 'page'];
    if (view.childLimit !== null) {
      fields.push('expanded');
    }
    return fields;
  }

  function genericView(form) {
    const collectionId = form.dataset.listControls;
    const pageSize = Number(form.dataset.pageSize);
    const childLimit = form.dataset.childLimit
      ? Number(form.dataset.childLimit)
      : null;
    if (!collectionId || !Number.isSafeInteger(pageSize) || pageSize < 1) {
      throw new Error(
        'rendered collection controls have invalid identity or page size',
      );
    }
    if (
      childLimit !== null &&
      (!Number.isSafeInteger(childLimit) || childLimit < 1)
    ) {
      throw new Error('rendered collection controls have invalid child limit');
    }
    const scope = form.closest('section');
    if (!scope) {
      throw new Error(`rendered collection ${collectionId} has no section`);
    }
    const inline = inlineCollectionRecords(scope, collectionId, pageSize);
    if (inline !== null && childLimit !== null) {
      throw new Error('an inline collection cannot contain grouped records');
    }
    const items = inline !== null
      ? inline.records
      : childLimit === null
        ? Array.from(scope.querySelectorAll('[data-list-item]')).map(
            (element, index) => genericRecord(element, index),
          )
        : Array.from(
            scope.querySelectorAll('[data-list-group]'),
            groupedRecord,
          );
    const children =
      childLimit === null
        ? []
        : items.flatMap(group =>
            Array.from(
              group.element.querySelectorAll(
                ':scope > a, :scope > .selected-cell-without-detail',
              ),
              (element, index) => groupedChildRecord(element, index, group),
            ),
          );
    const childrenByGroup = new Map();
    for (const item of items) {
      if (item.groupId) {
        childrenByGroup.set(
          item.groupId,
          children.filter(childRecord =>
            item.element.contains(childRecord.element),
          ),
        );
      }
    }
    const pagination = scope.querySelector(
      `[data-list-pagination="${collectionId}"]`,
    );
    const error = scope.querySelector(`[data-list-error="${collectionId}"]`);
    const empty = scope.querySelector(`[data-list-empty="${collectionId}"]`);
    if (!error || !empty) {
      throw new Error(
        `rendered collection ${collectionId} is missing status controls`,
      );
    }
    if (pagination === null && items.length > pageSize) {
      throw new Error(
        `rendered collection ${collectionId} omits required pagination`,
      );
    }
    const summary = pagination && pagination.querySelector('[data-list-summary]');
    const previous = pagination && pagination.querySelector('[data-list-previous]');
    const next = pagination && pagination.querySelector('[data-list-next]');
    if (pagination !== null && (!summary || !previous || !next)) {
      throw new Error(
        `rendered collection ${collectionId} has incomplete pagination`,
      );
    }
    const filterNames = genericFilterNames(form);
    const filterSources = Object.fromEntries(
      filterNames.map(name => [
        name,
        form.elements.namedItem(name).dataset.listSource || `value:${name}`,
      ]),
    );
    const filterValues = Object.fromEntries(
      filterNames.map(name => [name, selectValues(form, name)]),
    );
    const sortControl = form.elements.namedItem('sort');
    const sortValues = selectValues(form, 'sort');
    const sortSources = Object.fromEntries(
      Array.from(sortControl.options)
        .filter(option => option.value)
        .map(option => [
          option.value,
          option.dataset.listSource || `value:${option.value}`,
        ]),
    );
    const defaultSort = form.dataset.defaultSort;
    const defaultDirection = form.dataset.defaultDirection;
    const sortButtons = Array.from(scope.querySelectorAll('[data-list-sort]'));
    if (
      !sortValues.includes(defaultSort) ||
      !['asc', 'desc'].includes(defaultDirection) ||
      sortButtons.some(button => !sortValues.includes(button.dataset.listSort))
    ) {
      throw new Error(
        `rendered collection ${collectionId} has invalid defaults`,
      );
    }
    return {
      collectionId,
      queryPrefix: form.dataset.queryPrefix || '',
      form,
      items,
      children,
      childrenByGroup,
      parents:
        childLimit === null
          ? []
          : Array.from(scope.querySelectorAll('[data-list-parent]')),
      scope,
      pagination,
      summary,
      previous,
      next,
      error,
      empty,
      pageSize,
      childLimit,
      filterNames,
      filterSources,
      filterValues,
      sortSources,
      sortValues,
      defaultSort,
      defaultDirection,
      sortButtons,
      inlineTarget: inline === null ? null : inline.target,
      renderInlineRecord: inline === null ? null : inline.render,
      state: null,
      searchEditing: false,
    };
  }

  function genericAllowedNames(views) {
    const owners = new Map();
    for (const view of views) {
      for (const field of genericQueryFields(view)) {
        const name = genericQueryName(view, field);
        if (owners.has(name)) {
          throw new Error(`rendered collections repeat query name ${name}`);
        }
        owners.set(name, view.collectionId);
      }
    }
    return owners;
  }

  function parseGenericLocation(views) {
    const raw = root.location.search.startsWith('?')
      ? root.location.search.slice(1)
      : root.location.search;
    if (/%(?![0-9a-f]{2})/i.test(raw)) {
      return invalid('the URL contains an invalid percent escape');
    }
    const params = new URLSearchParams(raw);
    const owners = genericAllowedNames(views);
    const supplied = new Map();
    for (const [name, value] of params) {
      if (!owners.has(name)) {
        return invalid(`the URL contains unsupported filter ${name}`);
      }
      if (supplied.has(name)) {
        return invalid(`the URL repeats filter ${name}`);
      }
      supplied.set(name, value);
    }
    const states = new Map();
    for (const view of views) {
      const state = genericDefaults(view);
      const read = field => supplied.get(genericQueryName(view, field));
      const q = read('q');
      if (q !== undefined) {
        const trimmed = q.trim();
        if (!trimmed || trimmed.length > 256) {
          return invalid(
            `${genericQueryName(view, 'q')} must contain 1–256 characters`,
          );
        }
        state.q = trimmed;
      }
      for (const name of view.filterNames) {
        const value = read(name);
        if (value === undefined) {
          continue;
        }
        if (!value || !view.filterValues[name].includes(value)) {
          return invalid(
            `${genericQueryName(view, name)} has unsupported value ${value || '(empty)'}`,
          );
        }
        state.filters[name] = value;
      }
      const sort = read('sort');
      if (sort !== undefined) {
        if (!view.sortValues.includes(sort)) {
          return invalid(
            `${genericQueryName(view, 'sort')} has unsupported value ${sort}`,
          );
        }
        state.sort = sort;
      }
      const direction = read('direction');
      if (direction !== undefined) {
        if (!['asc', 'desc'].includes(direction)) {
          return invalid(
            `${genericQueryName(view, 'direction')} has unsupported value ${direction}`,
          );
        }
        state.direction = direction;
      }
      const page = read('page');
      if (page !== undefined) {
        if (
          !/^[1-9][0-9]*$/.test(page) ||
          !Number.isSafeInteger(Number(page))
        ) {
          return invalid(
            `${genericQueryName(view, 'page')} must be a positive integer`,
          );
        }
        state.page = Number(page);
      }
      const expanded = read('expanded');
      if (expanded !== undefined) {
        const values = expanded.split(',');
        const allowed = new Set(
          view.items.map(item => item.groupId).filter(Boolean),
        );
        if (
          !expanded ||
          new Set(values).size !== values.length ||
          values.some(value => !allowed.has(value))
        ) {
          return invalid(
            `${genericQueryName(view, 'expanded')} has unsupported groups`,
          );
        }
        state.expanded = new Set(values);
      }
      states.set(view.collectionId, state);
    }
    return {ok: true, states};
  }

  function genericStateFromControls(view) {
    const state = genericDefaults(view);
    state.q = view.form.elements.namedItem('q').value.trim();
    for (const name of view.filterNames) {
      state.filters[name] = view.form.elements.namedItem(name).value;
    }
    state.sort = view.form.elements.namedItem('sort').value;
    state.direction = view.form.elements.namedItem('direction').value;
    return state;
  }

  function setGenericControls(view, state) {
    view.form.elements.namedItem('q').value = state.q;
    for (const name of view.filterNames) {
      view.form.elements.namedItem(name).value = state.filters[name];
    }
    view.form.elements.namedItem('sort').value = state.sort;
    view.form.elements.namedItem('direction').value = state.direction;
  }

  function updateGenericSortButtons(view, state) {
    for (const button of view.sortButtons) {
      const active = button.dataset.listSort === state.sort;
      const heading = button.closest('th');
      const indicator = button.querySelector('.sort-indicator');
      if (!heading || !indicator) {
        throw new Error('table sort heading is incomplete');
      }
      heading.setAttribute(
        'aria-sort',
        active ? (state.direction === 'asc' ? 'ascending' : 'descending') : 'none',
      );
      indicator.textContent = active ? (state.direction === 'asc' ? '▲' : '▼') : '';
    }
  }

  function applyDependentFilterTransaction(view, state) {
    const plans = view.filterNames.map(name => {
      const filters = {...state.filters, [name]: ''};
      const probe = {...state, filters};
      const available = new Set(
        view.items
          .filter(record => genericMatches(record, probe, view))
          .flatMap(record => {
            const value = genericRecordValue(record, view.filterSources[name]);
            return Array.isArray(value) ? value.map(String) : [String(value ?? '')];
          })
          .filter(Boolean),
      );
      return {
        available,
        control: view.form.elements.namedItem(name),
        selected: state.filters[name],
      };
    });
    for (const plan of plans) {
      for (const option of plan.control.options) {
        option.disabled = Boolean(
          option.value &&
          option.value !== plan.selected &&
          !plan.available.has(option.value)
        );
      }
    }
  }

  function appendGenericQuery(params, view, state) {
    const baseline = genericDefaults(view);
    if (state.q) {
      params.append(genericQueryName(view, 'q'), state.q);
    }
    for (const name of view.filterNames) {
      if (state.filters[name]) {
        params.append(genericQueryName(view, name), state.filters[name]);
      }
    }
    if (state.sort !== baseline.sort) {
      params.append(genericQueryName(view, 'sort'), state.sort);
    }
    if (state.direction !== baseline.direction) {
      params.append(genericQueryName(view, 'direction'), state.direction);
    }
    if (state.page !== 1) {
      params.append(genericQueryName(view, 'page'), String(state.page));
    }
    if (state.expanded.size) {
      params.append(
        genericQueryName(view, 'expanded'),
        Array.from(state.expanded).sort(stringCompare).join(','),
      );
    }
  }

  function updateGenericUrl(views, push) {
    const url = new URL(root.location.href);
    const params = new URLSearchParams();
    for (const view of views) {
      appendGenericQuery(params, view, view.state || genericDefaults(view));
    }
    url.search = params.toString();
    commitUrl(url, push);
  }

  function genericStatesEqual(view, left, right) {
    if (left === null || right === null) {
      return false;
    }
    const leftParams = new URLSearchParams();
    const rightParams = new URLSearchParams();
    appendGenericQuery(leftParams, view, left);
    appendGenericQuery(rightParams, view, right);
    return leftParams.toString() === rightParams.toString();
  }

  function genericValueMatches(actual, expected) {
    if (Array.isArray(actual)) {
      return actual.map(String).includes(expected);
    }
    return String(actual ?? '') === expected;
  }

  function genericRecordValue(record, source) {
    if (source === 'ordinal') {
      return record.ordinal;
    }
    if (source === 'text') {
      return record.element === null
        ? record.search.trim()
        : record.element.textContent.trim();
    }
    if (source.startsWith('cell:')) {
      if (record.element === null) {
        throw new Error('an inline collection cannot read rendered table cells');
      }
      const index = Number(source.slice('cell:'.length));
      const cell = record.element.children.item(index);
      if (!cell) {
        throw new Error(`rendered collection item is missing cell ${index}`);
      }
      return cell.textContent.trim();
    }
    if (source.startsWith('value:')) {
      return record.values[source.slice('value:'.length)];
    }
    throw new Error(
      `rendered collection has unsupported value source ${source}`,
    );
  }

  function genericMatches(record, state, view) {
    if (
      state.q &&
      !record.search.toLowerCase().includes(state.q.toLowerCase())
    ) {
      return false;
    }
    return Object.entries(state.filters).every(
      ([name, value]) =>
        !value ||
        genericValueMatches(
          genericRecordValue(record, view.filterSources[name]),
          value,
        ),
    );
  }

  function genericValueIsMissing(value) {
    return value === undefined || value === null || value === '';
  }

  function compareGenericValues(left, right) {
    const leftMissing = genericValueIsMissing(left);
    const rightMissing = genericValueIsMissing(right);
    if (leftMissing !== rightMissing) {
      return leftMissing ? 1 : -1;
    }
    if (leftMissing) {
      return 0;
    }
    if (typeof left === 'number' && typeof right === 'number') {
      return left - right;
    }
    const leftText = String(left);
    const rightText = String(right);
    if (
      /^-?(?:0|[1-9][0-9]*)$/.test(leftText) &&
      /^-?(?:0|[1-9][0-9]*)$/.test(rightText)
    ) {
      const leftNumber = BigInt(leftText);
      const rightNumber = BigInt(rightText);
      return leftNumber < rightNumber ? -1 : leftNumber > rightNumber ? 1 : 0;
    }
    return stringCompare(leftText, rightText);
  }

  function sortGenericRecords(records, state, view) {
    return records.slice().sort((left, right) => {
      const source = view.sortSources[state.sort];
      const leftValue = genericRecordValue(left, source);
      const rightValue = genericRecordValue(right, source);
      const leftMissing = genericValueIsMissing(leftValue);
      const rightMissing = genericValueIsMissing(rightValue);
      if (leftMissing !== rightMissing) {
        return leftMissing ? 1 : -1;
      }
      const primary = compareGenericValues(
        leftValue,
        rightValue,
      );
      if (primary !== 0) {
        return state.direction === 'desc' ? -primary : primary;
      }
      return left.ordinal - right.ordinal;
    });
  }

  function removeRevealButtons(view) {
    for (const button of document.querySelectorAll('[data-list-reveal]')) {
      if (button.dataset.listReveal === view.collectionId) {
        button.remove();
      }
    }
  }

  function restoreGeneric(view, message) {
    removeRevealButtons(view);
    if (view.inlineTarget !== null) {
      view.inlineTarget.replaceChildren(
        ...view.items
          .slice()
          .sort((left, right) => left.ordinal - right.ordinal)
          .slice(0, view.pageSize)
          .map(view.renderInlineRecord),
      );
    } else {
      for (const record of view.items
        .slice()
        .sort((left, right) => left.ordinal - right.ordinal)) {
        record.parent.appendChild(record.element);
      }
    }
    for (const group of view.items) {
      for (const child of (view.childrenByGroup.get(group.groupId) || [])
        .slice()
        .sort((left, right) => left.ordinal - right.ordinal)) {
        group.element.appendChild(child.element);
      }
    }
    if (view.parents.length) {
      const container = view.parents[0].parentElement;
      for (const parent of view.parents) {
        container.appendChild(parent);
      }
    }
    for (const record of view.items.concat(view.children)) {
      if (record.element !== null) {
        record.element.hidden = false;
      }
    }
    for (const parent of view.parents) {
      parent.hidden = false;
    }
    view.form.reset();
    view.form.hidden = false;
    if (view.pagination !== null) {
      view.pagination.hidden = true;
    }
    view.empty.hidden = true;
    view.error.textContent = `Filters were not applied: ${message}.`;
    view.error.hidden = false;
    view.state = genericDefaults(view);
    updateGenericSortButtons(view, view.state);
  }

  function applySimpleGeneric(view, state) {
    const matching = sortGenericRecords(
      view.items.filter(record => genericMatches(record, state, view)),
      state,
      view,
    );
    const page = pageRecords(matching, state.page, view.pageSize);
    if (!page.ok) {
      return page;
    }
    if (view.inlineTarget !== null) {
      view.inlineTarget.replaceChildren(...page.records.map(view.renderInlineRecord));
    } else {
      const shown = new Set(page.records.map(record => record.element));
      for (const record of view.items) {
        record.element.hidden = !shown.has(record.element);
      }
      for (const record of matching) {
        record.parent.appendChild(record.element);
      }
    }
    return {ok: true, page, matching: matching.length, childCount: null};
  }

  function applyGroupedGeneric(view, state) {
    removeRevealButtons(view);
    if (view.parents.length) {
      const container = view.parents[0].parentElement;
      for (const parent of view.parents) {
        container.appendChild(parent);
      }
    }
    for (const group of view.items
      .slice()
      .sort((left, right) => left.ordinal - right.ordinal)) {
      group.parent.appendChild(group.element);
    }
    const groupMatches = [];
    let childCount = 0;
    for (const group of view.items) {
      const children = sortGenericRecords(
        (view.childrenByGroup.get(group.groupId) || []).filter(record =>
          genericMatches(record, state, view),
        ),
        state,
        view,
      );
      group.matchingChildren = children;
      if (children.length) {
        groupMatches.push(group);
        childCount += children.length;
      }
    }
    let matching = groupMatches;
    if (state.sort === 'hierarchy') {
      matching = sortGenericRecords(groupMatches, state, view);
    }
    const page = pageRecords(matching, state.page, view.pageSize);
    if (!page.ok) {
      return page;
    }
    const shown = new Set(page.records);
    for (const group of view.items) {
      group.element.hidden = !shown.has(group);
      for (const child of view.childrenByGroup.get(group.groupId) || []) {
        child.element.hidden = true;
      }
      if (!shown.has(group)) {
        continue;
      }
      const children = group.matchingChildren;
      const allChildren = view.childrenByGroup.get(group.groupId) || [];
      const expanded = state.expanded.has(group.groupId);
      const visible = expanded ? children : children.slice(0, view.childLimit);
      const visibleElements = new Set(visible.map(record => record.element));
      for (const child of allChildren) {
        child.element.hidden = !visibleElements.has(child.element);
      }
      const label = group.element.querySelector(':scope > strong');
      if (!label) {
        throw new Error(
          `rendered collection ${view.collectionId} group ${group.groupId} has no label`,
        );
      }
      group.element.replaceChildren(label);
      for (const [index, child] of visible.entries()) {
        group.element.appendChild(document.createTextNode(index === 0 ? ' ' : ', '));
        group.element.appendChild(child.element);
      }
      for (const child of allChildren) {
        if (!visibleElements.has(child.element)) {
          group.element.appendChild(child.element);
        }
      }
      if (children.length > view.childLimit) {
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'list-reveal';
        button.dataset.listReveal = view.collectionId;
        button.dataset.groupId = group.groupId;
        button.setAttribute('aria-expanded', String(expanded));
        button.textContent = expanded
          ? `Show first ${view.childLimit}`
          : `Show ${children.length - view.childLimit} more`;
        group.element.appendChild(button);
      }
    }
    const parentsShown = new Set(
      page.records.map(record => record.element.closest('[data-list-parent]')),
    );
    for (const parent of view.parents) {
      parent.hidden = !parentsShown.has(parent);
    }
    if (state.sort === 'hierarchy' && state.direction === 'desc') {
      const container = view.parents[0] && view.parents[0].parentElement;
      if (container) {
        for (const parent of view.parents.slice().reverse()) {
          container.appendChild(parent);
        }
      }
      for (const parent of view.parents) {
        const groups = view.items
          .filter(record => record.parent === parent)
          .reverse();
        for (const group of groups) {
          parent.appendChild(group.element);
        }
      }
    }
    return {ok: true, page, matching: matching.length, childCount};
  }

  function applyGeneric(view, state, historyAction, views) {
    const result =
      view.childLimit === null
        ? applySimpleGeneric(view, state)
        : applyGroupedGeneric(view, state);
    if (!result.ok) {
      restoreGeneric(view, result.error);
      return false;
    }
    const first = result.matching === 0 ? 0 : result.page.start + 1;
    const last = result.page.start + result.page.records.length;
    const childSummary =
      result.childCount === null ? '' : `; ${result.childCount} matching tests`;
    if (view.pagination !== null) {
      view.summary.textContent = `Showing ${first}–${last} of ${result.matching}${childSummary}; page ${state.page} of ${result.page.pages}.`;
      view.previous.disabled = state.page === 1;
      view.next.disabled = state.page === result.page.pages;
    } else if (result.page.pages !== 1) {
      throw new Error(`collection ${view.collectionId} requires pagination`);
    }
    view.empty.hidden = result.matching !== 0;
    view.error.hidden = true;
    if (view.pagination !== null) {
      view.pagination.hidden = result.page.pages <= 1;
    }
    view.form.hidden = false;
    view.state = state;
    setGenericControls(view, state);
    updateGenericSortButtons(view, state);
    applyDependentFilterTransaction(view, state);
    if (historyAction) {
      updateGenericUrl(views, historyAction === 'push');
    }
    return true;
  }

  function safelyApplyGeneric(view, state, historyAction, views) {
    try {
      return applyGeneric(view, state, historyAction, views);
    } catch (errorValue) {
      const message =
        errorValue instanceof Error ? errorValue.message : String(errorValue);
      restoreGeneric(view, message);
      return false;
    }
  }

  function bindGeneric(view, views) {
    const applyControls = historyAction => {
      const state = genericStateFromControls(view);
      state.page = 1;
      state.expanded = new Set();
      if (genericStatesEqual(view, view.state, state)) {
        setGenericControls(view, state);
        return true;
      }
      return safelyApplyGeneric(view, state, historyAction, views);
    };
    view.form.addEventListener('submit', event => {
      event.preventDefault();
      applyControls('push');
      view.searchEditing = false;
    });
    view.form.addEventListener('input', event => {
      if (event.target.name === 'q') {
        const action = view.searchEditing ? 'replace' : 'push';
        if (applyControls(action)) {
          view.searchEditing = true;
        }
      }
    });
    view.form.addEventListener('change', () => {
      applyControls('push');
      view.searchEditing = false;
    });
    view.form
      .querySelector('[data-list-clear]')
      .addEventListener('click', () => {
        const state = genericDefaults(view);
        view.form.reset();
        safelyApplyGeneric(view, state, 'push', views);
        view.searchEditing = false;
      });
    if (view.pagination !== null) {
      view.previous.addEventListener('click', event => {
        event.currentTarget.blur();
        const state = {...view.state, page: view.state.page - 1};
        safelyApplyGeneric(view, state, 'push', views);
        view.searchEditing = false;
      });
      view.next.addEventListener('click', event => {
        event.currentTarget.blur();
        const state = {...view.state, page: view.state.page + 1};
        safelyApplyGeneric(view, state, 'push', views);
        view.searchEditing = false;
      });
    }
    for (const button of view.sortButtons) {
      button.addEventListener('click', event => {
        const sort = event.currentTarget.dataset.listSort;
        const direction =
          view.state.sort === sort && view.state.direction === 'asc' ? 'desc' : 'asc';
        view.form.elements.namedItem('sort').value = sort;
        view.form.elements.namedItem('direction').value = direction;
        applyControls('push');
        view.searchEditing = false;
      });
    }
    const section = view.form.closest('section') || document;
    section.addEventListener('click', event => {
      const button = event.target.closest('[data-list-reveal]');
      if (!button || button.dataset.listReveal !== view.collectionId) {
        return;
      }
      const expanded = new Set(view.state.expanded);
      if (expanded.has(button.dataset.groupId)) {
        expanded.delete(button.dataset.groupId);
      } else {
        expanded.add(button.dataset.groupId);
      }
      safelyApplyGeneric(view, {...view.state, expanded}, 'push', views);
      view.searchEditing = false;
    });
  }

  function applyGenericLocation(views) {
    const parsed = parseGenericLocation(views);
    if (!parsed.ok) {
      for (const view of views) {
        restoreGeneric(view, parsed.error);
      }
      return;
    }
    for (const view of views) {
      const state = parsed.states.get(view.collectionId);
      safelyApplyGeneric(view, state, false, views);
    }
  }

  function initializeGeneric() {
    const forms = Array.from(document.querySelectorAll('[data-list-controls]'));
    if (!forms.length) {
      return;
    }
    let views = [];
    try {
      views = forms.map(genericView);
      genericAllowedNames(views);
      for (const view of views) {
        bindGeneric(view, views);
      }
      applyGenericLocation(views);
      root.addEventListener('popstate', () => {
        for (const view of views) {
          view.searchEditing = false;
        }
        applyGenericLocation(views);
      });
    } catch (errorValue) {
      const message =
        errorValue instanceof Error ? errorValue.message : String(errorValue);
      for (const view of views) {
        restoreGeneric(view, message);
      }
    }
  }

  function initializeCellListing(summary, urls, pageKind) {
    const form = document.querySelector('[data-cell-filter-form]');
    if (!form) {
      throw new Error('cell listing has no filter form');
    }
    form.hidden = false;
    const view = browserView(form, summary, urls, pageKind);
    bind(view);
    applyLocation(view);
  }

  const SITE_MANIFEST_PATH = 'data/site-manifest.json.gz';
  const SITE_SUMMARY_PATH = 'data/site-summary.json.gz';
  const DETAIL_PATHS = new Set(
    '0123456789abcdef'.split('').map(prefix => `data/detail-${prefix}.json.gz`),
  );
  const SITE_PAYLOAD_PATHS = new Set([SITE_SUMMARY_PATH, ...DETAIL_PATHS]);
  const DETAIL_PATTERN = /^[0-9a-f]{64}$/;
  const PAGE_SIZE = 50;
  let historyPushes = 0;

  function exactObject(value, keys, label) {
    if (!value || Array.isArray(value) || typeof value !== 'object') {
      throw new Error(`${label} is not an object`);
    }
    const actual = Object.keys(value).sort();
    const expected = [...keys].sort();
    if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
      throw new Error(`${label} has an unsupported shape`);
    }
    return value;
  }

  function fixedSiteUrls() {
    const location = root.location;
    if (location.protocol === 'file:') {
      throw new Error('local file URLs cannot load verified site data; use the site server');
    }
    const webProtocols = new Set(['h' + 'ttp:', 'h' + 'ttps:']);
    if (!webProtocols.has(location.protocol)) {
      throw new Error(`unsupported site protocol ${location.protocol}`);
    }
    const declaredRoot = document.body.dataset.siteRoot;
    const declaredManifest = document.body.dataset.siteManifest;
    if (!declaredRoot || !declaredManifest) {
      throw new Error('the page does not declare its fixed site data paths');
    }
    const rootPage = new URL(declaredRoot, location.href);
    const siteRoot = new URL('./', rootPage);
    const manifest = new URL(declaredManifest, location.href);
    const expectedManifest = new URL(SITE_MANIFEST_PATH, siteRoot);
    if (
      rootPage.origin !== location.origin ||
      manifest.origin !== location.origin ||
      manifest.href !== expectedManifest.href
    ) {
      throw new Error('the page data paths are not same-origin fixed paths');
    }
    return {manifest, siteRoot};
  }

  async function decodeResponseBytes(raw) {
    if (raw.length < 2 || raw[0] !== 0x1f || raw[1] !== 0x8b) {
      return raw;
    }
    if (typeof root.DecompressionStream !== 'function') {
      throw new Error('this browser cannot decode gzip site data');
    }
    const decodedStream = new Blob([raw])
      .stream()
      .pipeThrough(new root.DecompressionStream('gzip'));
    return new Uint8Array(await new Response(decodedStream).arrayBuffer());
  }

  async function requestDecoded(url) {
    const response = await root.fetch(url.href, {
      credentials: 'same-origin',
      redirect: 'error',
    });
    if (!response.ok) {
      throw new Error(`site data request failed with status ${response.status}`);
    }
    if (new URL(response.url).href !== url.href) {
      throw new Error('site data response changed its fixed URL');
    }
    const raw = new Uint8Array(await response.arrayBuffer());
    return decodeResponseBytes(raw);
  }

  function bytesToJson(bytes, label) {
    let text;
    try {
      text = new TextDecoder('utf-8', {fatal: true}).decode(bytes);
    } catch (_error) {
      throw new Error(`${label} is not valid UTF-8`);
    }
    let value;
    try {
      value = JSON.parse(text);
    } catch (_error) {
      throw new Error(`${label} is not valid JSON`);
    }
    text = null;
    return value;
  }

  async function sha256(bytes) {
    if (!root.crypto || !root.crypto.subtle) {
      throw new Error('this browser cannot verify SHA-256 site data');
    }
    const value = await root.crypto.subtle.digest('SHA-256', bytes);
    return Array.from(new Uint8Array(value), byte => byte.toString(16).padStart(2, '0')).join('');
  }

  async function loadManifest(urls) {
    const decoded = await requestDecoded(urls.manifest);
    const manifest = exactObject(
      bytesToJson(decoded, 'site manifest'),
      ['artifacts', 'schema_version'],
      'site manifest',
    );
    if (manifest.schema_version !== 1 || !Array.isArray(manifest.artifacts)) {
      throw new Error('site manifest schema is unsupported');
    }
    const artifacts = new Map();
    for (const raw of manifest.artifacts) {
      const item = exactObject(
        raw,
        ['decoded_bytes', 'decoded_sha256', 'path'],
        'site manifest artifact',
      );
      if (
        !SITE_PAYLOAD_PATHS.has(item.path) ||
        !Number.isSafeInteger(item.decoded_bytes) ||
        item.decoded_bytes < 0 ||
        !DETAIL_PATTERN.test(item.decoded_sha256) ||
        artifacts.has(item.path)
      ) {
        throw new Error('site manifest artifact is invalid');
      }
      artifacts.set(item.path, item);
    }
    if (
      artifacts.size !== SITE_PAYLOAD_PATHS.size ||
      [...SITE_PAYLOAD_PATHS].some(path => !artifacts.has(path))
    ) {
      throw new Error('site manifest does not contain the fixed payload allowlist');
    }
    return artifacts;
  }

  async function loadVerifiedPayload(urls, artifacts, path) {
    if (!SITE_PAYLOAD_PATHS.has(path)) {
      throw new Error('site data path is not allowlisted');
    }
    const metadata = artifacts.get(path);
    if (!metadata) {
      throw new Error('site data path is absent from the manifest');
    }
    const url = new URL(path, urls.siteRoot);
    if (url.origin !== root.location.origin || url.search || url.hash) {
      throw new Error('site data path is not a fixed same-origin URL');
    }
    let decoded = await requestDecoded(url);
    if (decoded.byteLength !== metadata.decoded_bytes) {
      throw new Error(`${path} decoded byte count does not match its manifest`);
    }
    if (await sha256(decoded) !== metadata.decoded_sha256) {
      throw new Error(`${path} decoded SHA-256 does not match its manifest`);
    }
    const value = bytesToJson(decoded, path);
    decoded = null;
    return value;
  }

  function yieldForVerifiedPayloadCleanup() {
    return new Promise(resolve => {
      const channel = new root.MessageChannel();
      channel.port1.onmessage = () => {
        channel.port1.close();
        channel.port2.close();
        resolve();
      };
      channel.port2.postMessage(null);
    });
  }

  function dataNode(tag, text, className) {
    const element = document.createElement(tag);
    if (text !== undefined && text !== null) {
      element.textContent = String(text);
    }
    if (className) {
      element.className = className;
    }
    return element;
  }

  function testName(test, category) {
    const identity = String(test);
    const prefix = `${category}/`;
    return identity.startsWith(prefix) ? identity.slice(prefix.length) : identity;
  }

  function displayResult(value) {
    if (value === 'pass') {
      return 'Pass';
    }
    if (value === 'product_failure') {
      return 'Fail';
    }
    if (value === null || value === '') {
      return 'Never measured';
    }
    throw new Error(`unsupported latest result ${value}`);
  }

  function yesOrNo(value, label) {
    if (typeof value !== 'boolean') {
      throw new Error(`${label} must be true or false`);
    }
    return value ? 'Yes' : 'No';
  }

  function commaSeparated(values, label) {
    if (!Array.isArray(values) || values.some(value => typeof value !== 'string')) {
      throw new Error(`${label} must be a list of strings`);
    }
    return values.length ? values.join(', ') : 'None';
  }

  function observationPolicy(observation) {
    if (
      !observation ||
      typeof observation !== 'object' ||
      typeof observation.status !== 'boolean' ||
      typeof observation.stdout !== 'boolean' ||
      typeof observation.stderr !== 'boolean' ||
      !Array.isArray(observation.artifacts) ||
      observation.artifacts.some(value => typeof value !== 'string')
    ) {
      throw new Error('test observation policy is malformed');
    }
    const included = [];
    const excluded = [];
    for (const [selected, label] of [
      [observation.status, 'exit status and terminating signal'],
      [observation.stdout, 'standard output bytes'],
      [observation.stderr, 'standard error bytes'],
    ]) {
      (selected ? included : excluded).push(label);
    }
    const sentences = [
      included.length
        ? `The observation hash includes ${included.join(', ')}.`
        : 'The observation hash includes none of the process result fields.',
      excluded.length ? `It does not include ${excluded.join(', ')}.` : '',
      observation.artifacts.length
        ? `It also includes bytes read from these artifact paths when present: ${observation.artifacts.join(', ')}.`
        : 'It includes no artifact files.',
    ];
    return sentences.filter(Boolean).join(' ');
  }

  function directCommand(direct) {
    if (direct === null) {
      return null;
    }
    if (!direct || typeof direct !== 'object') {
      throw new Error('test direct command is malformed');
    }
    if (direct.kind === 'shell' && typeof direct.command === 'string') {
      return dataNode('code', direct.command);
    }
    if (
      direct.kind === 'argv' &&
      Array.isArray(direct.argv) &&
      direct.argv.length > 0 &&
      direct.argv.every(value => typeof value === 'string')
    ) {
      return dataNode('code', JSON.stringify(direct.argv));
    }
    throw new Error('test direct command is malformed');
  }

  function easternTimeNode(value) {
    if (value === null || value === '') {
      return document.createTextNode('Not recorded');
    }
    if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|\+00:00)$/.test(value)) {
      throw new Error(`recorded timestamp is not UTC RFC3339: ${value}`);
    }
    const instant = new Date(value);
    if (Number.isNaN(instant.getTime())) {
      throw new Error(`recorded timestamp is invalid: ${value}`);
    }
    const formatter = new Intl.DateTimeFormat('en-US', {
      timeZone: 'America/New_York',
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
      hourCycle: 'h23',
    });
    const parts = Object.fromEntries(
      formatter.formatToParts(instant).map(part => [part.type, part.value]),
    );
    const node = dataNode(
      'time',
      `${parts.year}-${parts.month}-${parts.day} ${parts.hour}:${parts.minute}:${parts.second} ET`,
    );
    node.dateTime = value;
    return node;
  }

  function hermitCommitLink(commit) {
    if (!/^[0-9a-f]{40}$/.test(commit)) {
      throw new Error(`Hermit commit is not a full SHA: ${commit}`);
    }
    const link = dataNode('a');
    link.className = 'external-link';
    link.href = `${HERMIT_COMMIT_BASE}${commit}`;
    link.setAttribute('aria-label', `${commit} on GitHub (external link)`);
    link.appendChild(dataNode('code', commit));
    const indicator = dataNode('span', '↗', 'external-link-indicator');
    indicator.setAttribute('aria-hidden', 'true');
    link.appendChild(indicator);
    return link;
  }

  function appendStructured(parent, label, value) {
    const wrapper = dataNode('div', null, 'structured-value');
    wrapper.appendChild(dataNode('dt', label));
    const body = dataNode('dd');
    if (root.Node && value instanceof root.Node) {
      body.appendChild(value);
    } else if (Array.isArray(value)) {
      if (!value.length) {
        body.textContent = 'None';
      } else {
        const list = dataNode('ol');
        for (const item of value) {
          const entry = dataNode('li');
          if (item && typeof item === 'object') {
            const nested = dataNode('dl', null, 'facts compact-facts');
            for (const [name, nestedValue] of Object.entries(item)) {
              appendStructured(nested, name, nestedValue);
            }
            entry.appendChild(nested);
          } else {
            entry.textContent = String(item);
          }
          list.appendChild(entry);
        }
        body.appendChild(list);
      }
    } else if (value && typeof value === 'object') {
      const nested = dataNode('dl', null, 'facts compact-facts');
      for (const [name, nestedValue] of Object.entries(value)) {
        appendStructured(nested, name, nestedValue);
      }
      body.appendChild(nested);
    } else {
      body.textContent = value === null ? 'Not recorded' : String(value);
    }
    wrapper.appendChild(body);
    parent.appendChild(wrapper);
  }

  function safeSiteLink(urls, path, label) {
    const allowed = /^runs\/[0-9a-f]{64}\.html$/.test(path);
    if (!allowed) {
      throw new Error(`detail data contains unsupported link ${path}`);
    }
    const url = new URL(path, urls.siteRoot);
    if (url.origin !== root.location.origin) {
      throw new Error('detail link is not same-origin');
    }
    const link = dataNode('a', label);
    link.href = url.href;
    return link;
  }

  function collectionPageParameter(collectionId) {
    if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(collectionId)) {
      throw new Error('detail collection identity is invalid');
    }
    return `${collectionId.replaceAll('-', '_')}_page`;
  }

  function commitCollectionPage(collectionId, page, push) {
    const url = new URL(root.location.href);
    const parameter = collectionPageParameter(collectionId);
    if (page === 1) {
      url.searchParams.delete(parameter);
    } else {
      url.searchParams.set(parameter, String(page));
    }
    url.hash = '';
    commitUrl(url, push);
  }

  function collectionPageFromLocation(collectionId, records, pages, anchors) {
    const fragment = decodeURIComponent(root.location.hash.slice(1));
    if (fragment) {
      const index = records.findIndex(record =>
        anchors(record).includes(fragment),
      );
      if (index >= 0) {
        return Math.floor(index / PAGE_SIZE) + 1;
      }
    }
    const parameter = collectionPageParameter(collectionId);
    const values = new URLSearchParams(root.location.search).getAll(parameter);
    if (values.length > 1) {
      throw new Error(`detail URL repeats ${parameter}`);
    }
    if (!values.length) {
      return 1;
    }
    const value = values[0];
    const page = Number(value);
    if (
      !/^[1-9][0-9]*$/.test(value) ||
      !Number.isSafeInteger(page) ||
      page > pages
    ) {
      throw new Error(`${parameter} is not an available page`);
    }
    return page;
  }

  function pagedList(parent, records, noun, collectionId, renderRecord, anchors = () => []) {
    const caption = dataNode('p', `${records.length} ${noun}.`, 'data-caption');
    const list = dataNode('ol', null, 'browser-record-list');
    const pages = Math.max(1, Math.ceil(records.length / PAGE_SIZE));
    const navigation = pages > 1 ? dataNode('nav', null, 'list-pagination') : null;
    const previous = pages > 1 ? dataNode('button', 'Previous') : null;
    const summary = pages > 1 ? dataNode('span') : null;
    const next = pages > 1 ? dataNode('button', 'Next') : null;
    parent.append(caption, list);
    if (navigation !== null) {
      navigation.setAttribute('aria-label', `${noun} pages`);
      previous.type = 'button';
      summary.setAttribute('aria-live', 'polite');
      next.type = 'button';
      navigation.append(previous, summary, next);
      parent.appendChild(navigation);
    }
    let page = collectionPageFromLocation(
      collectionId,
      records,
      pages,
      anchors,
    );
    const draw = historyAction => {
      const start = (page - 1) * PAGE_SIZE;
      const shown = records.slice(start, start + PAGE_SIZE);
      list.replaceChildren(...shown.map(renderRecord));
      const first = shown.length ? start + 1 : 0;
      if (summary !== null) {
        summary.textContent = `Showing ${first}–${start + shown.length} of ${records.length}; page ${page} of ${pages}.`;
        previous.disabled = page === 1;
        next.disabled = page === pages;
      }
      if (historyAction) {
        commitCollectionPage(collectionId, page, historyAction === 'push');
      }
    };
    if (previous !== null) {
      previous.addEventListener('click', event => {
        event.currentTarget.blur();
        page = Math.max(1, page - 1);
        draw('push');
      });
      next.addEventListener('click', event => {
        event.currentTarget.blur();
        page = Math.min(pages, page + 1);
        draw('push');
      });
    }
    root.addEventListener('popstate', () => {
      page = collectionPageFromLocation(
        collectionId,
        records,
        pages,
        anchors,
      );
      draw(null);
    });
    draw(null);
  }

  function derivedCellState(cell) {
    if (!Number.isSafeInteger(cell.qualifying_physical_row_count) || cell.qualifying_physical_row_count < 0) {
      throw new Error('cell qualifying evidence count is invalid');
    }
    if (cell.qualifying_physical_row_count === 0) {
      if (cell.latest_result !== null) {
        throw new Error('zero qualifying evidence cannot have a latest result');
      }
      return 'Never measured';
    }
    if (cell.latest_result === 'pass') {
      return 'Green';
    }
    if (cell.latest_result === 'product_failure') {
      return 'Red';
    }
    throw new Error('qualifying evidence does not establish a browser result');
  }

  function runDisplayLabel(run) {
    const runNumber = run.run_number;
    if (
      (typeof runNumber === 'string' && /^[1-9][0-9]*$/.test(runNumber)) ||
      (Number.isSafeInteger(runNumber) && runNumber > 0)
    ) {
      return `Run ${runNumber}`;
    }
    return `Run id ${run.run_id}`;
  }

  function evidenceContext(item, urls) {
    const context = dataNode('p', null, 'evidence-context');
    if (item.run) {
      context.appendChild(safeSiteLink(urls, item.run.path, runDisplayLabel(item.run)));
    } else {
      context.appendChild(
        dataNode('strong', `Run id ${item.run_id} (no exact validation row link)`),
      );
    }
    const runIndex = item.series && item.series.run_index;
    if (!Number.isSafeInteger(runIndex) || runIndex < 0) {
      throw new Error('recorded evidence has an invalid run index');
    }
    context.appendChild(document.createTextNode(` · run index ${runIndex}`));
    const attempt = item.series.attempt;
    if (attempt !== null && attempt !== undefined) {
      if (!Number.isSafeInteger(attempt) || attempt < 1) {
        throw new Error('recorded evidence has an invalid attempt');
      }
      context.appendChild(document.createTextNode(` · attempt ${attempt}`));
    }
    context.appendChild(document.createTextNode(` · recorded by ${item.producer} at `));
    context.appendChild(easternTimeNode(item.emitted_at));
    context.appendChild(document.createTextNode('.'));
    return context;
  }

  function renderCellDetail(rootElement, record, urls) {
    const cell = record.data;
    rootElement.appendChild(dataNode('h1', record.identity));
    const facts = dataNode('dl', null, 'facts');
    for (const [label, value] of [
      ['Latest result', displayResult(cell.latest_result)],
      ['Category', cell.category],
      ['Test name', testName(cell.test, cell.category)],
      ['Mode', cell.mode],
      ['Backend', cell.backend],
      ['Lane', cell.lane],
      ['Physical rows', cell.physical_row_count],
      ['Represented Hermit runs', cell.represented_run_count],
      ['Qualifying physical rows', cell.qualifying_physical_row_count],
      ['Qualifying represented Hermit runs', cell.qualifying_represented_run_count],
      [
        'Latest result time',
        cell.latest_result_time ? easternTimeNode(cell.latest_result_time) : 'Not recorded',
      ],
      [
        'Latest result tree',
        cell.latest_result_tree
          ? hermitCommitLink(cell.latest_result_tree)
          : 'Not recorded',
      ],
      ['Guest arguments', cell.guest_args],
      ['Timeout seconds', cell.timeout_seconds],
      ['Working directory', cell.workdir],
      ['Not applicable reason', cell.not_applicable_reason],
    ]) {
      appendStructured(facts, label, value);
    }
    rootElement.appendChild(facts);
    const reproducer = dataNode('section');
    reproducer.appendChild(dataNode('h2', 'Reproducer'));
    const reproducerFacts = dataNode('dl', null, 'facts');
    appendStructured(reproducerFacts, 'Current reproducer', cell.current_reproducer);
    appendStructured(
      reproducerFacts,
      'Reproducer unavailable reason',
      cell.current_reproducer_unavailable_reason,
    );
    reproducer.appendChild(reproducerFacts);
    rootElement.appendChild(reproducer);
    const evidence = dataNode('section');
    evidence.appendChild(dataNode('h2', 'Recorded evidence'));
    pagedList(evidence, record.evidence, 'recorded evidence rows', 'recorded-evidence', item => {
      const row = dataNode('li', null, 'evidence-row');
      row.id = item.anchor;
      row.appendChild(evidenceContext(item, urls));
      const details = dataNode('details');
      details.appendChild(dataNode('summary', 'Audit fields'));
      const values = dataNode('dl', null, 'facts compact-facts');
      appendStructured(values, 'Event id', dataNode('code', item.event_id));
      appendStructured(values, 'Series', item.series);
      appendStructured(values, 'Product result', item.product_result);
      details.appendChild(values);
      row.appendChild(details);
      return row;
    }, item => [item.anchor]);
    rootElement.appendChild(evidence);
  }

  function recentPassingExecutionSection(cells, urls) {
    const candidates = cells
      .filter(cell => cell.recent_passing_execution !== null)
      .sort((left, right) => {
        const time = stringCompare(
          String(left.latest_result_time || ''),
          String(right.latest_result_time || ''),
        );
        return time || stringCompare(left.cell, right.cell);
      });
    if (!candidates.length) {
      return null;
    }
    const cell = candidates[candidates.length - 1];
    const execution = cell.recent_passing_execution;
    exactObject(
      execution,
      [
        'attempt',
        'emitted_at',
        'event_id',
        'info_messages_run1',
        'info_messages_run2',
        'run',
        'run_index',
        'syscalls_run1',
        'syscalls_run2',
      ],
      'recent passing execution',
    );
    for (const name of [
      'info_messages_run1',
      'info_messages_run2',
      'run_index',
      'syscalls_run1',
      'syscalls_run2',
    ]) {
      if (!Number.isSafeInteger(execution[name]) || execution[name] < 0) {
        throw new Error(`recent passing execution ${name} is invalid`);
      }
    }
    if (execution.run_index < 0) {
      throw new Error('recent passing execution run_index is invalid');
    }
    if (
      execution.attempt !== null &&
      execution.attempt !== undefined &&
      (!Number.isSafeInteger(execution.attempt) || execution.attempt < 1)
    ) {
      throw new Error('recent passing execution attempt is invalid');
    }
    if (typeof execution.event_id !== 'string' || !execution.event_id) {
      throw new Error('recent passing execution event_id is invalid');
    }

    const section = dataNode('section');
    section.appendChild(dataNode('h2', 'Recent passing execution with recorded counts'));
    const facts = dataNode('dl', null, 'facts compact-facts');
    const cellLabel = dataNode('code', `${cell.mode} · ${cell.backend}`);
    const cellLink = dataNode('a');
    cellLink.href = verifiedDetailUrl(urls, 'cell', cell.digest, cell.url);
    cellLink.appendChild(cellLabel);
    appendStructured(facts, 'Cell', cellLink);
    appendStructured(
      facts,
      'Validation run',
      safeSiteLink(urls, execution.run.path, runDisplayLabel(execution.run)),
    );
    appendStructured(facts, 'Recorded', easternTimeNode(execution.emitted_at));
    appendStructured(facts, 'Run index', execution.run_index);
    if (execution.attempt !== null && execution.attempt !== undefined) {
      appendStructured(facts, 'Attempt', execution.attempt);
    }
    section.appendChild(facts);
    const pairedCountsTable = (caption, label, left, right) => {
      const table = dataNode('table', null, 'execution-counts');
      table.appendChild(dataNode('caption', caption));
      const head = dataNode('thead');
      const headRow = dataNode('tr');
      for (const headingLabel of ['Count', 'Run 1', 'Run 2']) {
        const heading = dataNode('th', headingLabel);
        heading.scope = 'col';
        headRow.appendChild(heading);
      }
      head.appendChild(headRow);
      const body = dataNode('tbody');
      const row = dataNode('tr');
      const heading = dataNode('th', label);
      heading.scope = 'row';
      row.appendChild(heading);
      row.appendChild(dataNode('td', left, 'execution-count'));
      row.appendChild(dataNode('td', right, 'execution-count'));
      body.appendChild(row);
      table.append(head, body);
      return table;
    };
    const countGroups = dataNode('div', null, 'execution-count-groups');
    const comparisonGroup = dataNode('div', null, 'execution-count-group');
    comparisonGroup.appendChild(
      pairedCountsTable(
        'Canonical comparison population',
        'INFO messages',
        execution.info_messages_run1,
        execution.info_messages_run2,
      ),
    );
    comparisonGroup.appendChild(
      dataNode(
        'p',
        'These are the INFO-message populations evaluated by the canonical comparison.',
        'secondary',
      ),
    );
    const activityGroup = dataNode('div', null, 'execution-count-group');
    activityGroup.appendChild(
      pairedCountsTable(
        'Execution activity totals',
        'Hermit syscalls',
        execution.syscalls_run1,
        execution.syscalls_run2,
      ),
    );
    activityGroup.appendChild(
      dataNode(
        'p',
        'Hermit syscall totals describe execution activity. They may differ between runs and are not comparison evidence.',
        'secondary',
      ),
    );
    countGroups.append(comparisonGroup, activityGroup);
    section.appendChild(countGroups);
    const audit = dataNode('details');
    audit.appendChild(dataNode('summary', 'Exact event identifier'));
    audit.appendChild(dataNode('code', execution.event_id));
    section.appendChild(audit);
    section.appendChild(
      dataNode(
        'p',
        'This snapshot records comparison counts, execution activity totals, and digests. It does not include stdout or stderr bytes, or a public INFO-log file.',
        'secondary',
      ),
    );
    return section;
  }

  function renderTestDetail(rootElement, record, summary, urls, includeNeverMeasured) {
    const test = record.data;
    rootElement.appendChild(dataNode('h1', testName(record.identity, test.category)));
    const facts = dataNode('dl', null, 'facts');
    appendStructured(facts, 'Description', test.description);
    appendStructured(facts, 'Category', test.category);
    appendStructured(facts, 'Lane', test.lane);
    if (test.program !== null) {
      if (typeof test.program !== 'string') {
        throw new Error('test program is malformed');
      }
      appendStructured(facts, 'Program', dataNode('code', test.program));
    }
    const command = directCommand(test.direct);
    if (command !== null) {
      appendStructured(facts, 'Direct command', command);
    }
    if (test.build !== null) {
      if (!test.build || typeof test.build !== 'object') {
        throw new Error('test build flags are malformed');
      }
      appendStructured(
        facts,
        'C compiler flags',
        commaSeparated(test.build.cflags, 'C compiler flags'),
      );
      appendStructured(
        facts,
        'Rust compiler flags',
        commaSeparated(test.build.rustflags, 'Rust compiler flags'),
      );
    }
    appendStructured(facts, 'Observation hash', observationPolicy(test.observation));
    appendStructured(facts, 'Occasional', yesOrNo(test.occasional, 'occasional'));
    appendStructured(
      facts,
      'Required host capabilities',
      commaSeparated(test.requires, 'required host capabilities'),
    );
    appendStructured(
      facts,
      'Preprocessors',
      commaSeparated(test.preprocessors, 'preprocessors'),
    );
    rootElement.appendChild(facts);
    const testCells = summary.cells.filter(cell => cell.test === record.identity);
    const neverMeasuredCount = testCells.filter(
      cell => derivedCellState(cell) === 'Never measured',
    ).length;
    const cells = includeNeverMeasured
      ? testCells
      : testCells.filter(cell => derivedCellState(cell) !== 'Never measured');
    const recentPassing = recentPassingExecutionSection(testCells, urls);
    if (recentPassing !== null) {
      rootElement.appendChild(recentPassing);
    }
    const section = dataNode('section');
    section.appendChild(dataNode('h2', 'Published cells'));
    if (neverMeasuredCount > 0) {
      const populationControl = dataNode('label', null, 'population-control');
      const include = dataNode('input');
      include.type = 'checkbox';
      include.checked = includeNeverMeasured;
      include.addEventListener('change', () => {
        const url = new URL(root.location.href);
        if (include.checked) {
          url.searchParams.set('include_never_measured', 'true');
        } else {
          url.searchParams.delete('include_never_measured');
        }
        url.searchParams.delete(collectionPageParameter('published-cells'));
        url.hash = '';
        root.location.assign(url.href);
      });
      populationControl.append(
        include,
        document.createTextNode(` Include ${neverMeasuredCount} never measured cells`),
      );
      section.appendChild(populationControl);
    }
    pagedList(section, cells, 'published cells', 'published-cells', cell => {
      const item = dataNode('li');
      const label = dataNode('code', `${cell.mode} · ${cell.backend}`);
      if (cell.url === null) {
        if (derivedCellState(cell) !== 'Never measured') {
          throw new Error('a measured cell has no detail URL');
        }
        item.appendChild(label);
      } else {
        const link = dataNode('a');
        link.href = verifiedDetailUrl(urls, 'cell', cell.digest, cell.url);
        link.appendChild(label);
        item.appendChild(link);
      }
      item.appendChild(
        document.createTextNode(` — Latest result: ${displayResult(cell.latest_result)}`),
      );
      return item;
    });
    rootElement.appendChild(section);
    if (record.omitted_cells.length) {
      const omissions = dataNode('section');
      omissions.appendChild(dataNode('h2', 'Deliberate projection omissions'));
      const matching = summary.omissions.filter(item => item.test === record.identity);
      pagedList(omissions, matching, 'omitted cells', 'omitted-cells', item => {
        const row = dataNode('li');
        row.id = item.anchor;
        row.appendChild(dataNode('code', `${item.mode} · ${item.backend}`));
        if (typeof item.display_reason !== 'string' || !item.display_reason) {
          throw new Error('omitted cell display reason is missing');
        }
        row.appendChild(document.createTextNode(` — ${item.display_reason} `));
        const references = dataNode('details');
        references.appendChild(
          dataNode(
            'summary',
            `${item.event_references.length} exact event ${item.event_references.length === 1 ? 'reference' : 'references'}`,
          ),
        );
        item.event_references.forEach((reference, index) => {
          const visibleReference = dataNode('span', `Recorded evidence ${index + 1}`);
          visibleReference.id = reference.anchor;
          row.appendChild(visibleReference);
          row.appendChild(document.createTextNode(' '));
          references.appendChild(
            dataNode('code', reference.event_id, 'audit-identifier'),
          );
          references.appendChild(document.createElement('br'));
        });
        row.appendChild(references);
        return row;
      }, item => [item.anchor, ...item.event_references.map(reference => reference.anchor)]);
      rootElement.appendChild(omissions);
    }
  }

  function verifiedDetailUrl(urls, kind, digest, path) {
    const parameter = kind === 'cell' ? 'cell' : 'test';
    const expected = `${kind}s/detail.html?${parameter}=${digest}`;
    if (path !== expected) {
      throw new Error(`${kind} detail URL does not match its identity digest`);
    }
    const url = new URL(path, urls.siteRoot);
    if (url.origin !== root.location.origin) {
      throw new Error(`${kind} detail URL is not same-origin`);
    }
    return url.href;
  }

  function appendTableCell(row, value, heading, className = '') {
    const cell = dataNode(heading ? 'th' : 'td');
    if (heading) {
      cell.scope = 'row';
    }
    if (className) {
      cell.className = className;
    }
    if (root.Node && value instanceof root.Node) {
      cell.appendChild(value);
    } else {
      cell.textContent = value === null || value === '' ? 'Not recorded' : String(value);
    }
    row.appendChild(cell);
  }

  function hydratedCellRow(cell, tests, urls) {
    const stateLabel = derivedCellState(cell);
    const state = stateLabel.toLowerCase().replace(' ', '-');
    const row = dataNode('tr');
    row.dataset.cellRow = '1';
    row.dataset.cell = cell.cell;
    row.dataset.test = cell.test;
    row.dataset.lane = cell.lane;
    row.dataset.category = cell.category;
    row.dataset.mode = cell.mode;
    row.dataset.backend = cell.backend;
    row.dataset.state = state;
    row.dataset.result = cell.latest_result || '';
    row.dataset.selectedByPaths = JSON.stringify(cell.selected_by_paths);
    row.dataset.evidence = 'complete';
    row.dataset.latestResultTime = cell.latest_result_time || '';
    row.dataset.latestResultTree = cell.latest_result_tree || '';
    row.dataset.search = [cell.cell, cell.test, cell.lane, cell.category, cell.mode, cell.backend].join(' ');
    row.dataset.seriesFilterFacets = JSON.stringify(cell.series_filter_facets);
    row.dataset.physicalRowCount = String(cell.physical_row_count);
    row.dataset.representedRunCount = String(cell.represented_run_count);

    const test = tests.get(cell.test);
    if (!test) {
      throw new Error(`cell ${cell.cell} refers to an unknown test`);
    }
    appendTableCell(row, cell.category, false);
    const label = dataNode('code', testName(cell.test, cell.category));
    if (cell.url === null) {
      if (stateLabel !== 'Never measured') {
        throw new Error('a measured cell has no detail URL');
      }
      appendTableCell(row, label, true);
    } else {
      const testLink = dataNode('a');
      testLink.href = verifiedDetailUrl(urls, 'cell', cell.digest, cell.url);
      testLink.appendChild(label);
      appendTableCell(row, testLink, true);
    }
    for (const value of [cell.mode, cell.backend, cell.lane]) {
      appendTableCell(row, value, false);
    }
    appendTableCell(row, displayResult(cell.latest_result), false);
    appendTableCell(
      row,
      cell.latest_result_time
        ? easternTimeNode(cell.latest_result_time)
        : 'Not recorded',
      false,
    );
    appendTableCell(
      row,
      cell.latest_result_tree
        ? hermitCommitLink(cell.latest_result_tree)
        : 'Not recorded',
      false,
    );
    appendTableCell(row, cell.physical_row_count, false);
    appendTableCell(row, cell.represented_run_count, false);
    return row;
  }

  function validateSummary(summary) {
    exactObject(
      summary,
      ['cells', 'counts', 'custom_commands', 'omissions', 'projection_audit', 'provenance', 'raw_artifacts', 'recorded_outside_manifest', 'runs', 'schema_version', 'tests'],
      'site summary',
    );
    if (
      summary.schema_version !== 1 ||
      !Array.isArray(summary.cells) ||
      !Array.isArray(summary.tests) ||
      !Array.isArray(summary.runs) ||
      !Array.isArray(summary.omissions) ||
      !Array.isArray(summary.recorded_outside_manifest)
    ) {
      throw new Error('site summary schema is unsupported');
    }
    return summary;
  }

  function detailQuery(kind) {
    const parameter = kind === 'cell' ? 'cell' : 'test';
    const raw = root.location.search.startsWith('?')
      ? root.location.search.slice(1)
      : root.location.search;
    if (/%(?![0-9a-f]{2})/i.test(raw)) {
      throw new Error('the detail URL contains an invalid percent escape');
    }
    const supplied = new Set();
    const pageParameters = new Set();
    let digest = null;
    let includeNeverMeasured = false;
    for (const [name, value] of new URLSearchParams(raw)) {
      if (supplied.has(name)) {
        throw new Error(`the detail URL repeats ${name}`);
      }
      supplied.add(name);
      if (name === parameter) {
        if (!DETAIL_PATTERN.test(value)) {
          throw new Error(
            `the detail URL must contain exactly one ?${parameter}=<64 lowercase hex>`,
          );
        }
        digest = value;
        continue;
      }
      if (kind === 'test' && name === 'include_never_measured') {
        if (value !== 'true') {
          throw new Error('include_never_measured must equal true when present');
        }
        includeNeverMeasured = true;
        continue;
      }
      const page = Number(value);
      if (
        !/^[a-z0-9]+(?:_[a-z0-9]+)*_page$/.test(name) ||
        !/^[1-9][0-9]*$/.test(value) ||
        !Number.isSafeInteger(page)
      ) {
        throw new Error(`the detail URL contains unsupported parameter ${name}`);
      }
      pageParameters.add(name);
    }
    if (digest === null) {
      throw new Error(`the detail URL must contain exactly ?${parameter}=<64 lowercase hex>`);
    }
    return {digest, includeNeverMeasured, pageParameters};
  }

  function validateDetailPageParameters(query, kind, record) {
    const collectionIds =
      kind === 'cell'
        ? ['recorded-evidence']
        : [
            'published-cells',
            ...(record.omitted_cells.length ? ['omitted-cells'] : []),
          ];
    const allowed = new Set(collectionIds.map(collectionPageParameter));
    for (const parameter of query.pageParameters) {
      if (!allowed.has(parameter)) {
        throw new Error(`the detail URL contains unsupported parameter ${parameter}`);
      }
    }
  }

  function showDataFailure(errorValue) {
    const status = document.querySelector('[data-load-status]');
    if (!status) {
      return;
    }
    const message = errorValue instanceof Error ? errorValue.message : String(errorValue);
    status.textContent = `Verified site data could not be loaded: ${message}.`;
    status.dataset.loadState = 'error';
    status.classList.add('data-status-error');
    status.setAttribute('aria-busy', 'false');
    status.hidden = false;
  }

  function disableDynamicPage() {
    for (const control of document.querySelectorAll('button, input, select')) {
      control.disabled = true;
    }
    for (const body of document.querySelectorAll(
      '#cell-results, #recorded-not-in-current-manifest-results',
    )) {
      body.replaceChildren();
    }
    for (const section of document.querySelectorAll(
      '[data-in-manifest-listing], #recorded-not-in-current-manifest',
    )) {
      section.hidden = true;
    }
    const detail = document.querySelector('[data-detail-content]');
    if (detail) {
      detail.replaceChildren();
    }
  }

  function showFileProtocolHelp() {
    disableDynamicPage();
    const status = document.querySelector('[data-load-status]');
    if (!status) {
      return;
    }
    status.textContent =
      'Downloaded file:// pages cannot load verified site data. Serve the output directory with: ' +
      'python3 -m http.server 8000 --bind 127.0.0.1 --directory /absolute/path/to/output. ' +
      'That command is a convenience preview and is not equivalent to the Rust server headers.';
    status.dataset.loadState = 'file-help';
    status.classList.add('data-status-error');
    status.setAttribute('aria-busy', 'false');
    status.hidden = false;
  }

  function markDataLoaded(status) {
    status.textContent = '';
    status.dataset.loadState = 'verified';
    status.classList.remove('data-status-ok');
    status.setAttribute('aria-busy', 'false');
    status.hidden = true;
  }

  async function initializeVerifiedData() {
    const kind = document.body.dataset.pageKind;
    const isDetail = kind === 'cell-detail' || kind === 'test-detail';
    const isCellList = kind === 'cell-list' || kind === 'never-list';
    if (!isDetail && !isCellList) {
      return;
    }
    const status = document.querySelector('[data-load-status]');
    if (!status) {
      throw new Error('dynamic page has no data loading status');
    }
    status.setAttribute('aria-busy', 'true');
    const detail = isDetail
      ? detailQuery(kind === 'cell-detail' ? 'cell' : 'test')
      : null;
    const urls = fixedSiteUrls();
    const artifacts = await loadManifest(urls);
    const summaryPayload = await loadVerifiedPayload(
      urls,
      artifacts,
      SITE_SUMMARY_PATH,
    );
    const shard = isDetail
      ? await loadVerifiedPayload(
          urls,
          artifacts,
          `data/detail-${detail.digest[0]}.json.gz`,
        )
      : null;
    await yieldForVerifiedPayloadCleanup();
    const summary = validateSummary(summaryPayload);
    if (isCellList) {
      initializeCellListing(summary, urls, kind);
      markDataLoaded(status);
      return;
    }
    exactObject(shard, ['records', 'schema_version'], 'detail shard');
    if (shard.schema_version !== 1 || !Array.isArray(shard.records)) {
      throw new Error('browser payload schema is unsupported');
    }
    const expectedKind = kind === 'cell-detail' ? 'cell' : 'test';
    const matches = shard.records.filter(
      record =>
        record.kind === expectedKind && record.digest === detail.digest,
    );
    if (matches.length !== 1) {
      throw new Error('detail identity does not resolve to exactly one record');
    }
    validateDetailPageParameters(detail, expectedKind, matches[0]);
    const target = document.querySelector('[data-detail-content]');
    target.replaceChildren();
    if (expectedKind === 'cell') {
      renderCellDetail(target, matches[0], urls);
    } else {
      renderTestDetail(
        target,
        matches[0],
        summary,
        urls,
        detail.includeNeverMeasured,
      );
    }
    markDataLoaded(status);
  }

  async function initialize() {
    const kind = document.body.dataset.pageKind;
    const dynamic =
      kind === 'cell-detail' ||
      kind === 'test-detail' ||
      kind === 'cell-list' ||
      kind === 'never-list';
    if (dynamic && root.location.protocol === 'file:') {
      showFileProtocolHelp();
      return;
    }
    try {
      await initializeVerifiedData();
    } catch (errorValue) {
      if (dynamic) {
        disableDynamicPage();
      }
      showDataFailure(errorValue);
    }
    initializeGeneric();
  }

  if (typeof document !== 'undefined') {
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', initialize, {once: true});
    } else {
      initialize();
    }
  }
})(globalThis);
