import assert from "node:assert/strict";
import test from "node:test";

import { buildReleaseNotes, parseReleaseNotes } from "./release-notes.mjs";

test("parseReleaseNotes converts titled bullet groups", () => {
  const notes = parseReleaseNotes(
    "# Rebellion 2\n\n## Highlights\n\n* Added planning.\n- Improved fleets.\n\n## Fixes\n\n* Fixed repairs.",
    "0.0.13",
  );

  assert.deepEqual(notes, {
    version: "0.0.13",
    sections: [
      {
        title: "Highlights",
        items: ["Added planning.", "Improved fleets."],
      },
      { title: "Fixes", items: ["Fixed repairs."] },
    ],
  });
});

test("parseReleaseNotes omits prose and empty sections", () => {
  const notes = parseReleaseNotes(
    "Automated build.\n\n* Untitled item.\n\n## Empty\n\nParagraph only.",
    "0.0.13",
  );

  assert.deepEqual(notes, { version: "0.0.13", sections: [] });
});

test("buildReleaseNotes includes published history through the current release", () => {
  const notes = buildReleaseNotes(
    "## Additions\n\n* Added current feature.\n\n## Fixes\n\n* Fixed current issue.",
    "0.0.21",
    [
      {
        tag_name: "v0.0.20",
        body: "## Fixes\n\n* Fixed previous issue.",
        draft: false,
        prerelease: false,
      },
      {
        tag_name: "v0.0.16",
        body: "## Additions\n\n* Added previous feature.",
        draft: false,
        prerelease: false,
      },
      {
        tag_name: "latest-macos",
        body: "## Fixes\n\n* Ignored alias.",
        draft: false,
        prerelease: true,
      },
      {
        tag_name: "v0.0.22",
        body: "## Fixes\n\n* Ignored newer release.",
        draft: false,
        prerelease: false,
      },
    ],
  );

  assert.deepEqual(notes, {
    version: "0.0.21",
    sections: [
      { title: "Additions", items: ["Added current feature."] },
      { title: "Fixes", items: ["Fixed current issue."] },
    ],
    releases: [
      {
        version: "0.0.16",
        sections: [{ title: "Additions", items: ["Added previous feature."] }],
      },
      {
        version: "0.0.20",
        sections: [{ title: "Fixes", items: ["Fixed previous issue."] }],
      },
      {
        version: "0.0.21",
        sections: [
          { title: "Additions", items: ["Added current feature."] },
          { title: "Fixes", items: ["Fixed current issue."] },
        ],
      },
    ],
  });
});

test("buildReleaseNotes excludes drafts and releases without usable notes", () => {
  const notes = buildReleaseNotes("## Fixes\n\n* Fixed current issue.", "0.0.21", [
    {
      tag_name: "v0.0.19",
      body: "No structured notes.",
      draft: false,
      prerelease: false,
    },
    {
      tag_name: "v0.0.20",
      body: "## Fixes\n\n* Draft fix.",
      draft: true,
      prerelease: false,
    },
  ]);

  assert.deepEqual(notes.releases, [
    {
      version: "0.0.21",
      sections: [{ title: "Fixes", items: ["Fixed current issue."] }],
    },
  ]);
});
