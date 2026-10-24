// TP-PLAYER-INPUT-01
import { test } from "node:test";
import assert from "node:assert/strict";
import { picturePoint, pointerBody, wheelAction } from "../pointer.mjs";

const rect = { left: 100, top: 50, width: 400, height: 200 };
const picture = { width: 640, height: 360 };

test("a click on the picture maps to the pictures own pixels", () => {
  assert.deepEqual(picturePoint(rect, 300, 150, picture), { x: 320, y: 180, width: 640, height: 360 });
  assert.deepEqual(picturePoint(rect, 100, 50, picture), { x: 0, y: 0, width: 640, height: 360 });
  assert.deepEqual(picturePoint(rect, 499.9, 249.9, picture), { x: 639, y: 359, width: 640, height: 360 });
});

test("a pointer outside the picture is not a pointer", () => {
  assert.equal(picturePoint(rect, 99, 60, picture), null, "left of the element");
  assert.equal(picturePoint(rect, 500, 60, picture), null, "the right edge is outside");
  assert.equal(picturePoint(rect, 200, 250, picture), null, "the bottom edge is outside");
  assert.equal(picturePoint(rect, 200, 100, { width: 0, height: 360 }), null, "no picture yet");
  assert.equal(picturePoint({ left: 0, top: 0, width: 0, height: 0 }, 0, 0, picture), null, "no element yet");
});

test("wheel direction follows the sign of deltaY", () => {
  assert.equal(wheelAction(-3), "wheel_up");
  assert.equal(wheelAction(120), "wheel_down");
  assert.equal(wheelAction(0), null);
  assert.deepEqual(
    pointerBody("w1:p3", "down", 2, { x: 1, y: 2, width: 3, height: 4 }),
    { pane: "w1:p3", kind: "down", button: 2, x: 1, y: 2, width: 3, height: 4 },
  );
});
