// Contact-sheet frame selection, frame persistence, enlarged preview, and JPEG share/copy.

let selectedFrame = 0;

// Initialize the contact sheet with the previously selected frame (or middle frame).
document.addEventListener('DOMContentLoaded', () => {
    if (document.querySelector('.thumb-frame')) {
        var initial = (typeof window.initialFrame !== 'undefined') ? window.initialFrame : 0;
        selectFrame(initial);
    }
});

/** Highlight the chosen thumbnail, update the enlarged preview, and persist the selection. */
function selectFrame(n) {
    selectedFrame = n;
    document.querySelectorAll('.thumb-frame').forEach(el => {
        var active = parseInt(el.dataset.frame, 10) === n;
        el.classList.toggle('border-blue-500', active);
        el.classList.toggle('border-transparent', !active);
    });

    // Update enlarged preview above the thumbnail strip.
    var enlarged = document.getElementById('enlarged');
    if (enlarged && window.captionId != null) {
        enlarged.src = '/thumb/' + window.captionId + '/' + n;
    }

    // Persist to server so search results show the chosen frame as preview.
    if (window.captionId != null) {
        fetch('/select/' + window.captionId + '/' + n, { method: 'POST' }).catch(() => {});
    }
}

/**
 * Fetch the full-size JPEG for (captionId, frameN), then:
 *   1. Web Share API — only in secure context (mobile/HTTPS)
 *   2. Clipboard API — only in secure context (desktop/localhost/HTTPS)
 *   3. Download fallback — always works, including HTTP over LAN
 *
 * Stages 1 and 2 are gated on window.isSecureContext so that on plain HTTP
 * over a LAN IP we never call the share/clipboard APIs (which would throw
 * an exception even if the navigator properties exist).
 */
async function handleJpeg(captionId, frameN) {
    const btn = document.getElementById('jpeg-btn');
    if (btn) btn.disabled = true;

    try {
        const res = await fetch(`/thumb/${captionId}/${frameN}`);
        if (!res.ok) {
            showToast('取得に失敗しました');
            return;
        }

        const blob = await res.blob();
        const filename = `caption_${captionId}_${frameN}.jpg`;

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
            await navigator.clipboard.write([
                new ClipboardItem({ 'image/jpeg': blob })
            ]);
            showToast('クリップボードにコピーしました');
            return;
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

/** Show a brief toast message that auto-dismisses after 2 seconds. */
function showToast(msg) {
    const el = document.getElementById('toast');
    if (!el) return;
    el.textContent = msg;
    el.classList.remove('hidden');
    setTimeout(() => el.classList.add('hidden'), 2000);
}
