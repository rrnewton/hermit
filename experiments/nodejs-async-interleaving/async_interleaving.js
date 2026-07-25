'use strict';

const {
  isMainThread,
  parentPort,
  workerData,
  Worker,
} = require('worker_threads');

function positiveInt(value, name) {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`${name} must be a positive integer`);
  }
  return parsed;
}

function workerMain() {
  const { id, rounds, gateBuffer } = workerData;
  const gate = new Int32Array(gateBuffer);
  let value = (0x9e3779b9 ^ id) >>> 0;

  parentPort.postMessage({ kind: 'ready', id });
  Atomics.wait(gate, 0, 0);

  function runRound(round) {
    const work = 2048 + ((id * 131 + round * 197) & 2047);
    for (let spin = 0; spin < work; spin += 1) {
      value = Math.imul(value ^ (spin + round), 0x45d9f3b) >>> 0;
      value = ((value << 7) | (value >>> 25)) >>> 0;
    }

    parentPort.postMessage({ kind: 'event', id, round });

    if (round + 1 < rounds) {
      setImmediate(runRound, round + 1);
    } else {
      parentPort.postMessage({ kind: 'done', id, checksum: value });
    }
  }

  runRound(0);
}

async function main() {
  const workerCount = positiveInt(process.argv[2] || '8', 'workers');
  const rounds = positiveInt(process.argv[3] || '24', 'rounds');
  const gateBuffer = new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT);
  const gate = new Int32Array(gateBuffer);
  const workers = [];
  const ready = new Set();
  const done = new Set();
  const exited = new Set();
  const nextRound = new Array(workerCount).fill(0);
  const checksums = new Array(workerCount);
  const trace = [];
  let started = false;
  let settled = false;

  await new Promise((resolve, reject) => {
    function fail(error) {
      if (!settled) {
        settled = true;
        reject(error);
      }
    }

    function maybeFinish() {
      if (done.size !== workerCount || exited.size !== workerCount || settled) {
        return;
      }

      const expectedEvents = workerCount * rounds;
      if (trace.length !== expectedEvents) {
        fail(new Error(
          `trace lost events: expected ${expectedEvents}, got ${trace.length}`
        ));
        return;
      }
      if (checksums.some((checksum) => !Number.isSafeInteger(checksum))) {
        fail(new Error('worker checksum missing'));
        return;
      }

      settled = true;
      console.log(
        `ASYNC_TRACE workers=${workerCount} rounds=${rounds} events=${trace.length}`
      );
      console.log(trace.join(',') + ',');
      resolve();
    }

    for (let id = 0; id < workerCount; id += 1) {
      const worker = new Worker(__filename, {
        workerData: { id, rounds, gateBuffer },
      });
      workers.push(worker);

      worker.on('message', (message) => {
        if (!message || message.id !== id) {
          fail(new Error(`invalid message from worker ${id}`));
          return;
        }

        if (message.kind === 'ready') {
          if (started || ready.has(id)) {
            fail(new Error(`duplicate or late ready message from worker ${id}`));
            return;
          }
          ready.add(id);
          if (ready.size === workerCount) {
            started = true;
            Atomics.store(gate, 0, 1);
            Atomics.notify(gate, 0, workerCount);
          }
          return;
        }

        if (message.kind === 'event') {
          if (!started || done.has(id) || message.round !== nextRound[id]) {
            fail(new Error(`out-of-order event from worker ${id}`));
            return;
          }
          trace.push(`${id}:${message.round}`);
          nextRound[id] += 1;
          return;
        }

        if (message.kind === 'done') {
          if (done.has(id) || nextRound[id] !== rounds) {
            fail(new Error(`early or duplicate completion from worker ${id}`));
            return;
          }
          done.add(id);
          checksums[id] = message.checksum;
          maybeFinish();
          return;
        }

        fail(new Error(`unknown message kind from worker ${id}`));
      });

      worker.on('error', fail);
      worker.on('exit', (code) => {
        if (code !== 0) {
          fail(new Error(`worker ${id} exited with status ${code}`));
          return;
        }
        exited.add(id);
        maybeFinish();
      });
    }
  });
}

if (isMainThread) {
  main().catch((error) => {
    console.error(error.stack || error);
    process.exitCode = 1;
  });
} else {
  workerMain();
}
