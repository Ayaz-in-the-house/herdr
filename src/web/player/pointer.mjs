// A pointer on the page's picture of a pane, in that picture's own pixels.
//
// The page shows a pane's picture at whatever size fits its window; the
// server wants a point it can scale into the pane without knowing the page.
// So the page names the point inside the picture it is showing — the encoded
// picture's width and height — and refuses anything outside the element,
// because a point clamped onto the edge is a press the user did not make.
// Pure on purpose: no DOM, no fetch. TP-PLAYER-INPUT-01

/** `{ x, y, width, height }` in picture pixels, or null when the pointer is outside. */
export function picturePoint(rect, clientX, clientY, picture) {
  if (!rect || !(rect.width > 0) || !(rect.height > 0)) return null;
  if (!picture || !(picture.width > 0) || !(picture.height > 0)) return null;
  const fx = (clientX - rect.left) / rect.width;
  const fy = (clientY - rect.top) / rect.height;
  if (!(fx >= 0 && fx < 1 && fy >= 0 && fy < 1)) return null;
  return {
    x: Math.min(Math.floor(fx * picture.width), picture.width - 1),
    y: Math.min(Math.floor(fy * picture.height), picture.height - 1),
    width: picture.width,
    height: picture.height,
  };
}

/** The wheel step a `deltaY` names, or null when it names none. */
export function wheelAction(deltaY) {
  if (deltaY < 0) return "wheel_up";
  if (deltaY > 0) return "wheel_down";
  return null;
}

/** The body the bridge's `/pointer` turns into `pane.send_pointer`. */
export function pointerBody(pane, kind, button, point) {
  return { pane, kind, button, ...point };
}
