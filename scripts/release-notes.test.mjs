import assert from "node:assert/strict";
import test from "node:test";

import { parseReleaseNotes } from "./release-notes.mjs";

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
