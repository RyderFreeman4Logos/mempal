// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

(() => {
    const darkThemes = ['ayu', 'navy', 'coal'];
    const lightThemes = ['light', 'rust'];

    const classList = document.getElementsByTagName('html')[0].classList;

    let lastThemeWasLight = true;
    for (const cssClass of classList) {
        if (darkThemes.includes(cssClass)) {
            lastThemeWasLight = false;
            break;
        }
    }

    const theme = lastThemeWasLight ? 'default' : 'dark';
    const renderMermaid = async () => {
        document.querySelectorAll('pre > code.language-mermaid').forEach((code) => {
            const pre = code.parentElement;
            if (!pre) {
                return;
            }

            const mermaidBlock = document.createElement('pre');
            mermaidBlock.className = 'mermaid';
            mermaidBlock.textContent = code.textContent;
            pre.replaceWith(mermaidBlock);
        });

        mermaid.initialize({ startOnLoad: false, theme });
        if (typeof mermaid.run === 'function') {
            await mermaid.run({ querySelector: '.mermaid' });
        } else {
            mermaid.init(undefined, document.querySelectorAll('.mermaid'));
        }
    };

    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', () => {
            renderMermaid();
        });
    } else {
        renderMermaid();
    }

    // Simplest way to make mermaid re-render the diagrams in the new theme is via refreshing the page

    for (const darkTheme of darkThemes) {
        document.getElementById(darkTheme)?.addEventListener('click', () => {
            if (lastThemeWasLight) {
                window.location.reload();
            }
        });
    }

    for (const lightTheme of lightThemes) {
        document.getElementById(lightTheme)?.addEventListener('click', () => {
            if (!lastThemeWasLight) {
                window.location.reload();
            }
        });
    }
})();
