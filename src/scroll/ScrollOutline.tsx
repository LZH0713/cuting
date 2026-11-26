/**
 * The border drawn around a region being scroll-captured. The window ignores
 * the mouse (the app underneath must receive the scrolling), so this page is
 * purely visual — and the capture filter excludes its own window, so the
 * border never ends up inside the stitched image.
 */
export function ScrollOutline() {
  return <div className="scroll-outline" />;
}
