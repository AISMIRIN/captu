// Contact-sheet frame selection, frame persistence, client-side subtitle
// compositing for the enlarged preview, and JPEG share/copy/download.
//
// The enlarged image is built in the browser: /full/{id}/{n}?sub=0 gives the raw
// frame, /sub/{id} gives a full-frame transparent PNG holding the caption, and a
// canvas draws one over the other.  Toggling the subtitle therefore costs no
// ffmpeg run and no request, and only one variant per frame is ever cached
// server-side.  The composite is handed to #enlarged as an object URL rather
// than left in a <canvas> so the browser's own "copy image" / "save image as"
// context menu operates on the composited result.

let selectedFrame = 0;

// Caption ID read from #contact-root[data-caption-id] on DOMContentLoaded.
let _captionId = null;

// Subtitle overlay state.  Deliberately not persisted: every page load starts
// with the subtitle shown, matching what the thumbnails below always show.
let _subOn = true;
// Decoded subtitle PNG, fetched at most once per page (null until loaded, and
// when the caption has no renderable subtitle).
let _subImg = null;
// True once /sub/{id} has returned an actual overlay; the toggle is hidden
// until then, and stays hidden when the caption has none.
let _subAvailable = false;
// True once /sub/{id} has been requested, so a 204 is not re-fetched per frame.
let _subLoaded = false;
// Guards against out-of-order renders when thumbnails are clicked rapidly.
let _renderToken = 0;
// Object URL currently assigned to #enlarged; revoked when replaced.
let _currentUrl = null;
// Bytes behind #enlarged, reused by handleJpeg() for share/copy/download.
let _currentBlob = null;
// Reused compositing canvas; also the source for the PNG the clipboard needs.
let _canvas = null;
// Raw frame currently drawn, kept so toggling does not refetch it.
let _baseBitmap = null;
let _baseBlob = null;
let _baseFrame = null;
// Cache-buster query parameter set after a recapture.  The image routes send no
// Cache-Control, so without this the browser may hand back a file the server
// has just deleted.
let _bust = '';

/** Append the current cache-buster to `url`, picking ? or & as needed. */
function withBust(url) {
    if (!_bust) return url;
    return url + (url.includes('?') ? '&' : '?') + _bust;
}

// Initialize the contact sheet: read context from data attributes, select initial frame.
document.addEventListener('DOMContentLoaded', () => {
    const root = document.getElementById('contact-root');
    if (root) {
        const parsed = parseInt(root.dataset.captionId, 10);
        _captionId = isNaN(parsed) ? null : parsed;
    }
    updateSubButtons();
    if (document.querySelector('.thumb-frame')) {
        const initial = root ? (parseInt(root.dataset.initialFrame, 10) || 0) : 0;
        // Initial highlight only — merely opening the page must not persist a
        // selection (that would mark the caption as generated in the filters).
        selectFrame(initial, false);
    }
});

/** Reset the enlarged preview to the loading skeleton state. */
function setEnlargedLoading() {
    var e = document.getElementById('enlarged');
    if (!e) return;
    e.classList.add('opacity-0');
    e.parentElement.classList.add('animate-pulse', 'bg-gray-700');
}

/**
 * Highlight the chosen thumbnail and update the enlarged preview.
 * Persists the selection to the server unless persist === false
 * (the initial page-load call must not create a thumbnails row).
 */
function selectFrame(n, persist) {
    selectedFrame = n;
    document.querySelectorAll('.thumb-frame').forEach(el => {
        var active = parseInt(el.dataset.frame, 10) === n;
        el.classList.toggle('border-blue-500', active);
        el.classList.toggle('border-transparent', !active);
    });

    renderEnlarged(n);

    // Persist to server so search results show the chosen frame as preview.
    if (persist !== false && _captionId != null) {
        fetch('/select/' + _captionId + '/' + n, { method: 'POST' }).catch(() => {});
    }
}

/** Reflect _subOn / _subAvailable in the toggle buttons. */
function updateSubButtons() {
    const row = document.getElementById('sub-toggle-row');
    if (row) row.classList.toggle('hidden', !_subAvailable);

    const pairs = [
        [document.getElementById('sub-on'), _subOn],
        [document.getElementById('sub-off'), !_subOn],
    ];
    for (const [btn, active] of pairs) {
        if (!btn) continue;
        btn.classList.toggle('bg-blue-600', active);
        btn.classList.toggle('text-white', active);
        btn.classList.toggle('text-gray-400', !active);
        btn.classList.toggle('hover:text-gray-200', !active);
    }
}

/** Switch the subtitle overlay on or off and redraw (no network, no ffmpeg). */
function setSub(on) {
    if (_subOn === on) return;
    _subOn = on;
    updateSubButtons();
    renderEnlarged(selectedFrame);
}

/**
 * Fetch and decode the subtitle PNG, at most once per page.
 *
 * A 204 means the caption has no renderable subtitle (no PES blob, empty
 * caption list, or a fully transparent render) — not an error.  _subAvailable
 * then stays false and the toggle stays hidden.  _subLoaded keeps a 204 from
 * being re-requested on every frame change.
 */
async function ensureSubImage() {
    if (_subLoaded) return _subImg;
    _subLoaded = true;
    try {
        const res = await fetch(withBust('/sub/' + _captionId));
        if (res.status === 204 || !res.ok) return null;
        const blob = await res.blob();
        _subImg = await createImageBitmap(blob);
        _subAvailable = true;
    } catch {
        _subImg = null;
    }
    updateSubButtons();
    return _subImg;
}

/**
 * Fetch the raw frame for `n`, reusing the already-decoded one when the frame
 * has not changed (which is what makes toggling free).
 */
async function ensureBaseFrame(n) {
    if (_baseFrame === n && _baseBitmap) return true;

    const res = await fetch(withBust('/full/' + _captionId + '/' + n + '?sub=0'));
    if (!res.ok) return false;

    const blob = await res.blob();
    const bitmap = await createImageBitmap(blob);

    if (_baseBitmap) _baseBitmap.close();
    _baseBitmap = bitmap;
    _baseBlob = blob;
    _baseFrame = n;
    return true;
}

/** Encode the compositing canvas, as a Promise (canvas.toBlob is callback-based). */
function canvasBlob(type, quality) {
    return new Promise(resolve => _canvas.toBlob(resolve, type, quality));
}

/**
 * Draw frame `n` (plus the subtitle when enabled) and assign the result to
 * #enlarged as an object URL.
 *
 * With the subtitle off, the server's JPEG bytes are used verbatim — no canvas
 * re-encode, so nothing is lost.  With it on, the composite is re-encoded once
 * at quality 0.95.
 */
async function renderEnlarged(n) {
    const enlarged = document.getElementById('enlarged');
    if (!enlarged || _captionId == null) return;

    setEnlargedLoading();
    const token = ++_renderToken;
    // Drop the previous bytes up front: a half-finished render must never let
    // handleJpeg() hand out something other than what is on screen.
    _currentBlob = null;

    try {
        const [ok] = await Promise.all([ensureBaseFrame(n), ensureSubImage()]);
        if (token !== _renderToken) return;
        if (!ok) {
            showToast('画像の取得に失敗しました');
            return;
        }

        if (!_canvas) _canvas = document.createElement('canvas');
        _canvas.width = _baseBitmap.width;
        _canvas.height = _baseBitmap.height;
        const ctx = _canvas.getContext('2d');
        ctx.clearRect(0, 0, _canvas.width, _canvas.height);
        ctx.drawImage(_baseBitmap, 0, 0);

        const withSub = _subOn && _subImg;
        if (withSub) {
            // The PNG is a full-frame canvas with the caption already at its
            // absolute position, so scaling it to the frame is all that is
            // needed — the same thing ffmpeg's overlay branch does.
            ctx.drawImage(_subImg, 0, 0, _canvas.width, _canvas.height);
        }

        const blob = withSub ? await canvasBlob('image/jpeg', 0.95) : _baseBlob;
        if (token !== _renderToken || !blob) return;

        if (_currentUrl) URL.revokeObjectURL(_currentUrl);
        _currentUrl = URL.createObjectURL(blob);
        _currentBlob = blob;
        enlarged.src = _currentUrl;
    } catch {
        if (token === _renderToken) showToast('画像の取得に失敗しました');
    }
}

/**
 * Hand the image currently shown in #enlarged to the platform:
 *   1. Web Share API — only in secure context (mobile/HTTPS)
 *   2. Clipboard API — only in secure context (desktop/localhost/HTTPS)
 *   3. Download fallback — always works, including HTTP over LAN
 *
 * Stages 1 and 2 are gated on window.isSecureContext so that on plain HTTP
 * over a LAN IP we never call the share/clipboard APIs (which would throw
 * an exception even if the navigator properties exist).
 *
 * The bytes come from renderEnlarged(), so what is shared/copied/saved always
 * matches what is on screen, subtitle toggle included.  The clipboard gets PNG
 * specifically: Chromium and Safari only guarantee image/png for
 * clipboard.write, and writing image/jpeg throws.
 */
async function handleJpeg(captionId, frameN) {
    const btn = document.getElementById('jpeg-btn');

    if (!_currentBlob) {
        showToast('画像を準備中です');
        return;
    }

    if (btn) btn.disabled = true;

    const blob = _currentBlob;
    const suffix = _subOn && _subImg ? '' : '_nosub';
    const filename = `caption_${captionId}_${frameN}${suffix}.jpg`;

    try {
        if (window.isSecureContext && navigator.share && navigator.canShare) {
            // Stage 1: secure context + Web Share API (mobile/HTTPS)
            const file = new File([blob], filename, { type: 'image/jpeg' });
            if (navigator.canShare({ files: [file] })) {
                await navigator.share({ files: [file] });
                return;
            }
        }

        if (window.isSecureContext && navigator.clipboard && navigator.clipboard.write) {
            // Stage 2: secure context + Clipboard API (desktop/localhost/HTTPS)
            const png = await canvasBlob('image/png');
            if (png) {
                await navigator.clipboard.write([
                    new ClipboardItem({ 'image/png': png })
                ]);
                showToast('クリップボードにコピーしました');
                return;
            }
        }

        // Stage 3: download fallback — works on plain HTTP over LAN
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = filename;
        document.body.appendChild(a);
        a.click();
        document.body.removeChild(a);
        URL.revokeObjectURL(url);
        showToast('画像を保存しました');
    } catch (e) {
        // User cancelled share — no toast. Any other error shows failure message.
        if (e.name !== 'AbortError') {
            showToast('コピーに失敗しました');
        }
    } finally {
        if (btn) btn.disabled = false;
    }
}

/**
 * Clear cached images for a caption and force the browser to reload them.
 *
 * POSTs to /recapture/:id (server deletes thumbs/full/sub PNGs), then drops the
 * client-side compositing state and re-renders.  The enlarged preview cannot be
 * cache-busted with ?v= any more — its src is an object URL — so it is rebuilt
 * from scratch instead.  The thumbnail strip is still plain server URLs and does
 * need the query-string bust.
 */
async function recapture(captionId) {
    showToast('画像を削除中...');
    try {
        const res = await fetch(`/recapture/${captionId}`, { method: 'POST' });
        if (!res.ok) {
            showToast('画像再作成に失敗しました');
            return;
        }

        // Discard everything derived from the deleted files.
        _bust = `v=${Date.now()}`;
        if (_baseBitmap) _baseBitmap.close();
        _baseBitmap = null;
        _baseBlob = null;
        _baseFrame = null;
        if (_subImg) _subImg.close();
        _subImg = null;
        _subLoaded = false;
        _subAvailable = false;
        updateSubButtons();

        renderEnlarged(selectedFrame);

        // Reload all thumbnail images in the contact-sheet grid.
        document.querySelectorAll('#thumb-grid img').forEach(img => {
            img.src = withBust(img.src.split('?')[0]);
        });

        showToast('画像を再作成しました');
    } catch {
        showToast('画像再作成に失敗しました');
    }
}

/** Show a brief toast message that auto-dismisses after 2 seconds. */
function showToast(msg) {
    const el = document.getElementById('toast');
    if (!el) return;
    el.textContent = msg;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 2000);
}
