import { pathToFileURL } from "node:url";

/** Compares two validated semantic versions without considering build metadata. */
export function compareSemanticVersions(left, right) {
  const leftVersion = parseSemanticVersion(left);
  const rightVersion = parseSemanticVersion(right);
  for (const field of ["major", "minor", "patch"]) {
    const comparison = compareBigInts(leftVersion[field], rightVersion[field]);
    if (comparison !== 0) return comparison;
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
  const core = match.slice(1, 4);
  const prerelease = match[4]?.split(".") ?? [];
  const build = match[5]?.split(".") ?? [];
  if (
    core.some((identifier) => hasLeadingZero(identifier)) ||
    prerelease.some(
      (identifier) =>
        !/^[0-9A-Za-z-]+$/u.test(identifier) ||
        (/^\d+$/u.test(identifier) && hasLeadingZero(identifier)),
    ) ||
    build.some((identifier) => !/^[0-9A-Za-z-]+$/u.test(identifier))
  ) {
    throw new Error(`Invalid semantic version: ${version}`);
  }
  return {
    major: BigInt(match[1]),
    minor: BigInt(match[2]),
    patch: BigInt(match[3]),
    prerelease,
  };
}

/** Returns whether a numeric semantic-version identifier has a forbidden leading zero. */
function hasLeadingZero(identifier) {
  return identifier.length > 1 && identifier.startsWith("0");
}

/** Compares arbitrary-size integers without losing precision. */
function compareBigInts(left, right) {
  return left === right ? 0 : left < right ? -1 : 1;
}

/** Compares one semantic-version prerelease identifier. */
function compareIdentifier(left, right) {
  if (left === right) return 0;
  const leftNumeric = /^\d+$/u.test(left);
  const rightNumeric = /^\d+$/u.test(right);
  if (leftNumeric && rightNumeric) {
    return compareBigInts(BigInt(left), BigInt(right));
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
