"use strict";

const queueData = [
  { id: "SR-2481", title: "Maritime corridor disruption expands into a second operating zone", region: "East Med", severity: "critical", age: "8 min", detail: "Eastern Mediterranean" },
  { id: "SR-2478", title: "Tier-one supplier reports unscheduled production halt", region: "Europe", severity: "high", age: "21 min", detail: "Central Europe" },
  { id: "SR-2475", title: "Credential campaign targets logistics control systems", region: "Global", severity: "critical", age: "37 min", detail: "Global operations" },
  { id: "SR-2471", title: "Regulatory action creates immediate reporting exposure", region: "N. America", severity: "high", age: "52 min", detail: "North America" },
  { id: "SR-2468", title: "Port congestion exceeds seasonal operating baseline", region: "APAC", severity: "medium", age: "1 hr", detail: "Asia Pacific" },
  { id: "SR-2464", title: "Cross-border payment latency signals liquidity pressure", region: "LATAM", severity: "medium", age: "2 hr", detail: "Latin America" },
  { id: "SR-2461", title: "Civil disruption affects two primary distribution routes", region: "Europe", severity: "high", age: "2 hr", detail: "Southern Europe" },
  { id: "SR-2457", title: "Cloud service degradation impacts identity providers", region: "Global", severity: "critical", age: "3 hr", detail: "Global operations" },
  { id: "SR-2452", title: "Drought outlook raises commodity supply uncertainty", region: "Africa", severity: "medium", age: "4 hr", detail: "East Africa" },
  { id: "SR-2449", title: "Sanctions update changes counterparty screening scope", region: "Europe", severity: "high", age: "5 hr", detail: "Eastern Europe" },
  { id: "SR-2443", title: "Localized power instability affects industrial zone", region: "M. East", severity: "medium", age: "6 hr", detail: "Middle East" },
  { id: "SR-2439", title: "Insurance capacity tightens across exposed shipping class", region: "Global", severity: "low", age: "8 hr", detail: "Global operations" },
  { id: "SR-2434", title: "Election timetable increases short-term policy volatility", region: "APAC", severity: "medium", age: "10 hr", detail: "South Asia" },
  { id: "SR-2428", title: "Waterway restrictions reduce daily freight throughput", region: "LATAM", severity: "high", age: "12 hr", detail: "Latin America" },
  { id: "SR-2423", title: "Labor action notice issued for strategic transport hub", region: "Europe", severity: "medium", age: "14 hr", detail: "Western Europe" },
  { id: "SR-2418", title: "Minor aftershock sequence remains within forecast range", region: "APAC", severity: "low", age: "16 hr", detail: "Asia Pacific" }
];

const signalData = [
  { time: "14:24", title: "Freight insurance spread widened 11 bps", region: "Eastern Mediterranean", severity: "critical" },
  { time: "13:58", title: "Third-party credential activity accelerated", region: "Global · Technology", severity: "high" },
  { time: "13:31", title: "Supplier outage estimate extended to 48 hours", region: "Central Europe", severity: "high" },
  { time: "12:47", title: "Port dwell time crossed monitoring threshold", region: "Asia Pacific", severity: "medium" }
];

const recentData = [
  { code: "R-08", title: "Maritime corridor risk raised to critical", meta: "8 min · East Med", severity: "critical" },
  { code: "R-14", title: "Supplier continuity score revised downward", meta: "21 min · Europe", severity: "high" },
  { code: "R-03", title: "Credential campaign linked to new cluster", meta: "37 min · Global", severity: "critical" }
];

const matrixData = {
  risk: {
    values: [0, 1, 1, 2, 3, 0, 1, 2, 4, 3, 1, 1, 3, 3, 2, 0, 2, 2, 1, 1, 0, 1, 1, 0, 0],
    counts: [0, 1, 1, 2, 3, 0, 1, 2, 5, 3, 1, 2, 4, 6, 2, 0, 3, 4, 2, 1, 0, 2, 1, 0, 0],
    priority: "11",
    insight: "Highest concentration: likely events with major operational impact."
  },
  velocity: {
    values: [0, 0, 2, 3, 4, 0, 1, 2, 3, 4, 0, 1, 2, 3, 3, 0, 1, 1, 2, 2, 0, 0, 1, 1, 1],
    counts: [0, 0, 1, 3, 2, 0, 1, 2, 4, 3, 0, 1, 3, 5, 4, 0, 1, 2, 3, 2, 0, 0, 1, 2, 1],
    priority: "09",
    insight: "Nine risks show accelerating velocity; four changed within the past hour."
  }
};

const pageSize = 4;
let currentPage = 1;
let selectedId = queueData[0].id;

const searchInput = document.querySelector("#risk-search");
const severityFilter = document.querySelector("#severity-filter");
const queueBody = document.querySelector("#queue-body");
const queueEmpty = document.querySelector("#queue-empty");
const timeline = document.querySelector("#signal-timeline");
const recentList = document.querySelector("#recent-list");

function normalizedQuery() {
  return searchInput.value.trim().toLocaleLowerCase();
}

function matchesFilters(item) {
  const query = normalizedQuery();
  const severity = severityFilter.value;
  const text = Object.values(item).join(" ").toLocaleLowerCase();
  return (severity === "all" || item.severity === severity) && (!query || text.includes(query));
}

function severityLabel(value) {
  return value.charAt(0).toUpperCase() + value.slice(1);
}

function updateHeadline(item) {
  if (!item) {
    selectedId = null;
    document.querySelector("#headline-severity").textContent = "No match";
    document.querySelector("#headline-title").textContent = "No intelligence matches the active filters";
    document.querySelector("#headline-meta").textContent = "Clear the search or select another severity to restore the briefing.";
    return;
  }
  selectedId = item.id;
  document.querySelector("#headline-severity").textContent = severityLabel(item.severity);
  document.querySelector("#headline-title").textContent = item.title;
  document.querySelector("#headline-meta").textContent = `${item.id} · ${item.detail} · Updated ${item.age} ago`;
}

function renderQueue() {
  const filtered = queueData.filter(matchesFilters);
  if (!filtered.some((item) => item.id === selectedId)) updateHeadline(filtered[0] || null);
  const totalPages = Math.max(1, Math.ceil(filtered.length / pageSize));
  currentPage = Math.min(currentPage, totalPages);
  const start = (currentPage - 1) * pageSize;
  const visible = filtered.slice(start, start + pageSize);

  queueBody.replaceChildren(...visible.map((item) => {
    const row = document.createElement("tr");
    row.dataset.id = item.id;
    row.classList.toggle("is-selected", item.id === selectedId);

    const idCell = document.createElement("td");
    idCell.textContent = item.id;
    const titleCell = document.createElement("td");
    titleCell.title = item.title;
    const titleButton = document.createElement("button");
    titleButton.type = "button";
    titleButton.className = "queue-row-button";
    titleButton.textContent = item.title;
    titleButton.setAttribute("aria-pressed", String(item.id === selectedId));
    titleButton.setAttribute("aria-label", `${item.id}: ${item.title}, ${severityLabel(item.severity)} severity`);
    titleCell.append(titleButton);
    const regionCell = document.createElement("td");
    regionCell.textContent = item.region;
    const severityCell = document.createElement("td");
    const severity = document.createElement("span");
    severity.className = `severity severity-${item.severity}`;
    severity.textContent = severityLabel(item.severity);
    severityCell.append(severity);
    const ageCell = document.createElement("td");
    ageCell.textContent = item.age;
    row.append(idCell, titleCell, regionCell, severityCell, ageCell);

    const selectRow = () => {
      updateHeadline(item);
      [...queueBody.children].forEach((candidate) => {
        const active = candidate.dataset.id === selectedId;
        candidate.classList.toggle("is-selected", active);
        candidate.querySelector(".queue-row-button")?.setAttribute("aria-pressed", String(active));
      });
    };
    row.addEventListener("click", selectRow);
    titleButton.addEventListener("click", (event) => {
      event.stopPropagation();
      selectRow();
    });
    return row;
  }));

  queueEmpty.hidden = filtered.length !== 0;
  document.querySelector("#queue-result-count").textContent = `${filtered.length} ${filtered.length === 1 ? "signal" : "signals"}`;
  document.querySelector("#current-page").textContent = String(currentPage);
  document.querySelector("#total-pages").textContent = String(totalPages);
  document.querySelector("#queue-previous").disabled = currentPage === 1;
  document.querySelector("#queue-next").disabled = currentPage === totalPages;
}

function renderTimeline() {
  const filtered = signalData.filter(matchesFilters);
  timeline.replaceChildren(...filtered.map((item) => {
    const entry = document.createElement("li");
    entry.className = "signal-item";
    entry.dataset.severity = item.severity;
    entry.innerHTML = `<time>${item.time} UTC</time><h3>${item.title}</h3><p>${severityLabel(item.severity)} · ${item.region}</p>`;
    return entry;
  }));
  document.querySelector("#timeline-empty").hidden = filtered.length !== 0;
}

function renderRecent() {
  const filtered = recentData.filter(matchesFilters);
  recentList.replaceChildren(...filtered.map((item) => {
    const entry = document.createElement("li");
    entry.className = "recent-item";
    entry.innerHTML = `<span class="recent-code">${item.code}</span><div><h3>${item.title}</h3><p>${item.meta}</p></div>`;
    return entry;
  }));
  document.querySelector("#recent-empty").hidden = filtered.length !== 0;
}

function applyFilters() {
  currentPage = 1;
  renderQueue();
  renderTimeline();
  renderRecent();
}

function renderMatrix(mode) {
  const dataset = matrixData[mode];
  const heatmap = document.querySelector("#heatmap");
  heatmap.replaceChildren(...dataset.values.map((level, index) => {
    const cell = document.createElement("div");
    const impact = 5 - Math.floor(index / 5);
    const likelihood = (index % 5) + 1;
    cell.className = "heat-cell";
    cell.dataset.level = String(level);
    cell.setAttribute("role", "img");
    const levelNames = ["baseline", "low", "moderate", "high", "critical"];
    cell.setAttribute("aria-label", `Impact ${impact}, likelihood ${likelihood}, ${dataset.counts[index]} risks, ${levelNames[level]}`);
    if (dataset.counts[index] > 0) cell.innerHTML = `<span>${dataset.counts[index]}</span>`;
    return cell;
  }));
  document.querySelector("#matrix-priority").textContent = dataset.priority;
  document.querySelector("#matrix-insight").textContent = dataset.insight;
}

function setActiveView(targetId, scroll = true) {
  document.querySelectorAll(".view-link").forEach((button) => {
    const active = button.dataset.target === targetId;
    button.classList.toggle("is-active", active);
    if (active) button.setAttribute("aria-current", "page");
    else button.removeAttribute("aria-current");
  });
  if (scroll) document.querySelector(`#${targetId}`)?.scrollIntoView({ block: "start" });
}

const viewButtons = [...document.querySelectorAll(".view-link")];
const viewIds = new Set(viewButtons.map((button) => button.dataset.target));

viewButtons.forEach((button) => {
  button.addEventListener("click", () => setActiveView(button.dataset.target));
});

function syncViewFromHash() {
  const targetId = window.location.hash.slice(1);
  if (viewIds.has(targetId)) setActiveView(targetId, false);
}

window.addEventListener("hashchange", syncViewFromHash);
syncViewFromHash();

searchInput.addEventListener("input", applyFilters);
severityFilter.addEventListener("change", applyFilters);

document.querySelector("#reset-filters").addEventListener("click", () => {
  searchInput.value = "";
  severityFilter.value = "all";
  applyFilters();
  searchInput.focus();
});

document.querySelector("#queue-previous").addEventListener("click", () => {
  currentPage = Math.max(1, currentPage - 1);
  renderQueue();
});

document.querySelector("#queue-next").addEventListener("click", () => {
  const pages = Math.max(1, Math.ceil(queueData.filter(matchesFilters).length / pageSize));
  currentPage = Math.min(pages, currentPage + 1);
  renderQueue();
});

const matrixTabs = [...document.querySelectorAll(".matrix-tab")];
matrixTabs.forEach((tab, tabIndex) => {
  tab.addEventListener("click", () => {
    matrixTabs.forEach((candidate) => {
      const active = candidate === tab;
      candidate.classList.toggle("is-active", active);
      candidate.setAttribute("aria-selected", String(active));
      candidate.tabIndex = active ? 0 : -1;
    });
    document.querySelector("#matrix-view").setAttribute("aria-labelledby", tab.id);
    renderMatrix(tab.dataset.matrix);
  });
  tab.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    const direction = event.key === "ArrowRight" ? 1 : -1;
    const nextTab = matrixTabs[(tabIndex + direction + matrixTabs.length) % matrixTabs.length];
    nextTab.click();
    nextTab.focus();
  });
});

document.querySelector("#refresh-dashboard").addEventListener("click", () => {
  const now = new Date();
  const time = document.querySelector("#last-updated");
  time.dateTime = now.toISOString();
  time.textContent = `${now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false })} local`;
  renderQueue();
  renderTimeline();
  renderRecent();
});

document.querySelector("#download-report").addEventListener("click", () => {
  const report = {
    product: "Sentinel Risk Intelligence",
    dataClassification: "Illustrative static data",
    generatedAt: new Date().toISOString(),
    posture: { status: "Elevated", score: 72, trendPercent: 8 },
    activeFilters: { query: searchInput.value, severity: severityFilter.value },
    intelligence: queueData.filter(matchesFilters),
    emergingSignals: signalData.filter(matchesFilters),
    regionalExposure: { Europe: 82, AsiaPacific: 74, MiddleEast: 61, NorthAmerica: 46, LatinAmerica: 33 }
  };
  const blobUrl = URL.createObjectURL(new Blob([JSON.stringify(report, null, 2)], { type: "application/json" }));
  const link = document.createElement("a");
  link.href = blobUrl;
  link.download = `sentinel-risk-report-${new Date().toISOString().slice(0, 10)}.json`;
  document.body.append(link);
  link.click();
  link.remove();
  window.setTimeout(() => URL.revokeObjectURL(blobUrl), 0);
});

renderQueue();
renderTimeline();
renderRecent();
renderMatrix("risk");
