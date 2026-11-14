.pragma library

// The panel's few surface strengths, in one place. Every colour on the panel is one of the
// theme's foreground, accent or urgent colours at one of these opacities; none is written out.
var fill = 0.04;        // a card resting on the panel
var fillHover = 0.07;   // a row or tab under the pointer
var fillStrong = 0.09;  // the chosen card, a callout
var line = 0.12;        // card outlines and rules
var lineStrong = 0.45;  // the chosen card's outline
var track = 0.10;       // the empty part of a meter
var quiet = 0.38;       // a meter part that is not the subject

function tint(c, a) {
    return Qt.rgba(c.r, c.g, c.b, a);
}
