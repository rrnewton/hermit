// Resolve exact pull-request heads covered by the trusted Reverie-pin gate.
//
// actions/github-script loads this module from the trusted main checkout. A
// pull_request event needs no API lookup; dispatch and merge-queue events use
// the authenticated Octokit client supplied by the action. No ambient `gh`
// executable participates in the authority chain.

'use strict';

const SHA_RE = /^[0-9a-f]{40}$/;
const MAIN_BRANCH = 'main';
const MAIN_REF = 'refs/heads/main';

function refuse(message) {
  throw new Error(`reverie-pin target resolution refused: ${message}`);
}

function requireSha(value, field) {
  if (typeof value !== 'string' || !SHA_RE.test(value)) {
    refuse(`${field} must be a lowercase 40-hex commit SHA`);
  }
  return value;
}

function requireNumber(value, field) {
  const text = String(value ?? '');
  if (!/^[1-9][0-9]*$/.test(text)) {
    refuse(`${field} must be a positive pull-request number`);
  }
  const number = Number(text);
  if (!Number.isSafeInteger(number)) {
    refuse(`${field} is outside the safe integer range`);
  }
  return number;
}

function repositoryParts(context, expectedRepository) {
  if (!/^[^/]+\/[^/]+$/.test(expectedRepository)) {
    refuse('expected repository must be an owner/name pair');
  }
  const contextRepository = `${context.repo?.owner ?? ''}/${context.repo?.repo ?? ''}`;
  if (contextRepository !== expectedRepository) {
    refuse(`workflow repository ${contextRepository} is not ${expectedRepository}`);
  }
  const payloadRepository = context.payload?.repository?.full_name;
  if (payloadRepository !== expectedRepository) {
    refuse(`event repository ${payloadRepository ?? '<missing>'} is not ${expectedRepository}`);
  }
  const [owner, repo] = expectedRepository.split('/');
  return {owner, repo};
}

function validatePullRequest(pr, options) {
  if (!pr || typeof pr !== 'object') {
    refuse('pull_request object is missing');
  }

  const number = requireNumber(pr.number, 'pull_request.number');
  if (options.expectedNumber !== undefined && number !== options.expectedNumber) {
    refuse(`pull_request.number ${number} does not match expected ${options.expectedNumber}`);
  }
  if (pr.state !== 'open') {
    refuse(`pull request #${number} is not open`);
  }
  if (pr.base?.ref !== MAIN_BRANCH) {
    refuse(`pull request #${number} base ${pr.base?.ref ?? '<missing>'} is not ${MAIN_BRANCH}`);
  }
  if (pr.base?.repo?.full_name !== options.expectedRepository) {
    refuse(
      `pull request #${number} base repository ${pr.base?.repo?.full_name ?? '<missing>'} ` +
        `is not ${options.expectedRepository}`,
    );
  }

  const headSha = requireSha(pr.head?.sha, `pull request #${number} head`);
  if (options.expectedHeadSha !== undefined && headSha !== options.expectedHeadSha) {
    refuse(
      `pull request #${number} head ${headSha} does not match dispatched head ` +
        options.expectedHeadSha,
    );
  }
  if (options.expectedHeadRef !== undefined && pr.head?.ref !== options.expectedHeadRef) {
    refuse(
      `pull request #${number} head ref ${pr.head?.ref ?? '<missing>'} does not match ` +
        options.expectedHeadRef,
    );
  }
  if (
    options.expectedHeadRepository !== undefined &&
    pr.head?.repo?.full_name !== options.expectedHeadRepository
  ) {
    refuse(
      `pull request #${number} head repository ${pr.head?.repo?.full_name ?? '<missing>'} ` +
        `is not ${options.expectedHeadRepository}`,
    );
  }

  return {number, headSha, baseRef: MAIN_BRANCH};
}

function resolvePullRequest(context, expectedRepository) {
  const eventNumber = requireNumber(context.payload?.number, 'event pull-request number');
  return [
    validatePullRequest(context.payload?.pull_request, {
      expectedRepository,
      expectedNumber: eventNumber,
    }),
  ];
}

async function resolveWorkflowDispatch(github, context, expectedRepository, owner, repo) {
  const number = requireNumber(
    context.payload?.inputs?.pr_number,
    'workflow_dispatch input pr_number',
  );
  const expectedHeadSha = requireSha(context.sha, 'workflow_dispatch SHA');
  if (typeof context.ref !== 'string' || !context.ref.startsWith('refs/heads/')) {
    refuse('workflow_dispatch must run from the pull-request head branch');
  }
  const expectedHeadRef = context.ref.slice('refs/heads/'.length);

  const response = await github.rest.pulls.get({owner, repo, pull_number: number});
  return [
    validatePullRequest(response?.data, {
      expectedRepository,
      expectedNumber: number,
      expectedHeadSha,
      expectedHeadRef,
      expectedHeadRepository: expectedRepository,
    }),
  ];
}

async function resolveMergeGroup(github, context, expectedRepository, owner, repo) {
  const mergeGroup = context.payload?.merge_group;
  if (!mergeGroup || typeof mergeGroup !== 'object') {
    refuse('merge_group object is missing');
  }
  if (mergeGroup.base_ref !== MAIN_REF) {
    refuse(`merge-group base ${mergeGroup.base_ref ?? '<missing>'} is not ${MAIN_REF}`);
  }
  const queueHeadSha = requireSha(mergeGroup.head_sha, 'merge-group head');
  if (requireSha(context.sha, 'workflow SHA') !== queueHeadSha) {
    refuse(`workflow SHA ${context.sha} does not match merge-group head ${queueHeadSha}`);
  }
  if (context.ref !== mergeGroup.head_ref) {
    refuse(
      `workflow ref ${context.ref ?? '<missing>'} does not match merge-group head ref ` +
        `${mergeGroup.head_ref ?? '<missing>'}`,
    );
  }

  const query = `
    query($owner: String!, $name: String!, $branch: String!) {
      repository(owner: $owner, name: $name) {
        mergeQueue(branch: $branch) {
          entries(first: 100) {
            nodes {
              headCommit { oid }
              pullRequest {
                number
                state
                baseRefName
                baseRepository { nameWithOwner }
                headRefOid
              }
            }
          }
        }
      }
    }
  `;
  const data = await github.graphql(query, {owner, name: repo, branch: MAIN_BRANCH});
  const nodes = data?.repository?.mergeQueue?.entries?.nodes;
  if (!Array.isArray(nodes)) {
    refuse('merge queue lookup returned no entries collection');
  }

  const targets = [];
  const seen = new Set();
  for (const entry of nodes) {
    if (entry?.headCommit?.oid !== queueHeadSha) {
      continue;
    }
    const pr = entry.pullRequest;
    if (
      pr?.state !== 'OPEN' ||
      pr?.baseRefName !== MAIN_BRANCH ||
      pr?.baseRepository?.nameWithOwner !== expectedRepository
    ) {
      continue;
    }
    const number = requireNumber(pr.number, 'merge-queue pull-request number');
    if (seen.has(number)) {
      refuse(`merge queue returned duplicate pull request #${number}`);
    }
    seen.add(number);
    targets.push({
      number,
      headSha: requireSha(pr.headRefOid, `merge-queue pull request #${number} head`),
      baseRef: MAIN_BRANCH,
    });
  }

  if (targets.length === 0) {
    refuse(
      `no open ${expectedRepository}:${MAIN_BRANCH} merge-queue entry is bound to ${queueHeadSha}`,
    );
  }
  targets.sort((left, right) => left.number - right.number);
  return targets;
}

async function resolveTargets({github, context, expectedRepository}) {
  if (!github || typeof github !== 'object') {
    refuse('authenticated GitHub client is missing');
  }
  if (!context || typeof context !== 'object') {
    refuse('GitHub Actions context is missing');
  }
  const {owner, repo} = repositoryParts(context, expectedRepository);

  switch (context.eventName) {
    case 'pull_request':
      return resolvePullRequest(context, expectedRepository);
    case 'workflow_dispatch':
      return resolveWorkflowDispatch(github, context, expectedRepository, owner, repo);
    case 'merge_group':
      return resolveMergeGroup(github, context, expectedRepository, owner, repo);
    default:
      refuse(`unsupported event ${context.eventName ?? '<missing>'}`);
  }
}

module.exports = {resolveTargets};
