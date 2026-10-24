// The suite pins its own wiring: a node test nobody runs is green forever.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";

const NODE_LINE = "node --test src/web/player/test/*.test.mjs";

// TP-PLAYER-SUITE-01
test("every player test file is run by the check recipe", () => {
  const justfile = readFileSync(new URL("../../../../justfile", import.meta.url), "utf8");
  const recipes = ["\ncheck: ci windows-lint\n", "\ntest:\n"];
  for (const recipe of recipes) {
    const start = justfile.indexOf(recipe);
    assert.ok(start >= 0, `recipe ${recipe.trim()} is missing`);
    const body = justfile.slice(start + 1).split(/\n(?=\S)/)[0];
    assert.ok(body.includes(NODE_LINE), `${recipe.trim()} does not run ${NODE_LINE}`);
  }
  // The glob names every file here; a file that does not match it is a test
  // the recipe never sees.
  const here = new URL("./", import.meta.url);
  const files = readdirSync(here).filter((name) => name.endsWith(".mjs"));
  assert.ok(files.length >= 1);
  for (const name of files) assert.match(name, /\.test\.mjs$/, `${name} is outside the recipe's glob`);
});
