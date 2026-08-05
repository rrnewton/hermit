// Integrity-checked production loader for the trusted Reverie-pin resolver.
//
// This file is intentionally not wired into merge-gate.yml yet. Bootstrap PR 1
// lands the loader, resolver, and brackets on main. A later reviewed cutover can
// load both files from trusted main (or an immutable commit) without executing
// helper bytes supplied by the pull request under evaluation.

'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const Module = require('node:module');
const path = require('node:path');

const EXPECTED_RESOLVER_SHA256 =
  'f8d58ef89bbd31fcea3f6e326fd15e3819231f5dc6131079e34ca988df4f2d69';

class ResolverIntegrityError extends Error {
  constructor(message) {
    super(message);
    this.name = 'ResolverIntegrityError';
  }
}

function loadResolver(resolverPath) {
  if (typeof resolverPath !== 'string' || resolverPath.length === 0) {
    throw new ResolverIntegrityError('resolver path must be a non-empty string');
  }

  const absolutePath = path.resolve(resolverPath);
  const bytes = fs.readFileSync(absolutePath);
  const actualDigest = crypto.createHash('sha256').update(bytes).digest('hex');
  if (actualDigest !== EXPECTED_RESOLVER_SHA256) {
    throw new ResolverIntegrityError(
      `resolver digest ${actualDigest} does not match trusted ${EXPECTED_RESOLVER_SHA256}`,
    );
  }

  // Compile the exact bytes just hashed. Reopening through require() would add
  // a check/use race on a mutable runner filesystem.
  const loaded = new Module(absolutePath);
  loaded.filename = absolutePath;
  loaded.paths = Module._nodeModulePaths(path.dirname(absolutePath));
  loaded._compile(bytes.toString('utf8'), absolutePath);
  if (typeof loaded.exports?.resolveTargets !== 'function') {
    throw new ResolverIntegrityError('trusted resolver does not export resolveTargets');
  }
  return loaded.exports;
}

module.exports = {
  EXPECTED_RESOLVER_SHA256,
  ResolverIntegrityError,
  loadResolver,
};
