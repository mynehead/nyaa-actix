// Loads the ALTCHA widget and its SHA-256 worker from this site, so the strict CSP
// (scripts and workers from 'self' only) holds. See templates/macros.html.
import '/static/altcha/altcha.min.js';

globalThis.$altcha.algorithms.set('SHA-256', () => new Worker('/static/altcha/sha.js'));
