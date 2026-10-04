import { pathToFileURL } from "node:url";

/** Compares two validated semantic versions without considering build metadata. */
export function compareSemanticVersions(left, right) {
  const leftVersion = parseSemanticVersion(left);
  const rightVersion = parseSemanticVersion(right);
  for (const field of ["major", "minor", "patch"]) {
    const difference = leftVersion[field] - rightVersion[field];
    if (difference !== 0) return Math.sign(difference);
  }

  const leftIdentifiers = leftVersion.prerelease;
  const rightIdentifiers = rightVersion.prerelease;
  if (leftIdentifiers.length === 0 || rightIdentifiers.length === 0) {
    return leftIdentifiers.length === rightIdentifiers.length
      ? 0
      : leftIdentifiers.length === 0
        ? 1
        : -1;
  }

  const length = Math.max(leftIdentifiers.length, rightIdentifiers.length);
  for (let index = 0; index < length; index += 1) {
    if (index >= leftIdentifiers.length) return -1;
    if (index >= rightIdentifiers.length) return 1;
    const comparison = compareIdentifier(
      leftIdentifiers[index],
      rightIdentifiers[index],
    );
    if (comparison !== 0) return comparison;
  }
  return 0;
}

/** Parses the subset of semantic versions accepted by the release workflows. */
function parseSemanticVersion(version) {
  const match = /^(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$/u.exec(
    version,
  );
  if (!match) throw new Error(`Invalid semantic version: ${version}`);
  return {
    major: Number.parseInt(match[1], 10),
    minor: Number.parseInt(match[2], 10),
    patch: Number.parseInt(match[3], 10),
    prerelease: match[4]?.split(".") ?? [],
  };
}

/** Compares one semantic-version prerelease identifier. */
function compareIdentifier(left, right) {
  if (left === right) return 0;
  const leftNumeric = /^\d+$/u.test(left);
  const rightNumeric = /^\d+$/u.test(right);
  if (leftNumeric && rightNumeric) {
    return Math.sign(Number.parseInt(left, 10) - Number.parseInt(right, 10));
  }
  if (leftNumeric) return -1;
  if (rightNumeric) return 1;
  return left < right ? -1 : 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [left, right] = process.argv.slice(2);
  if (!left || !right) {
    throw new Error("Usage: node scripts/semver.mjs <left> <right>");
  }
  process.stdout.write(`${compareSemanticVersions(left, right)}\n`);
}
