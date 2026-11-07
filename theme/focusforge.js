// SPDX-License-Identifier: GPL-3.0-or-later
// The manual offers the two FocusForge themes only: Light, and Dark on mdBook's
// "coal" slot. The other built-in themes are removed from the theme menu.
(function () {
    "use strict";
    for (const id of ["rust", "navy", "ayu"]) {
        const button = document.getElementById("mdbook-theme-" + id);
        if (button && button.parentElement) {
            button.parentElement.remove();
        }
    }
    const dark = document.getElementById("mdbook-theme-coal");
    if (dark) {
        dark.textContent = "Dark";
    }
})();
