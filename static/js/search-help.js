// Search patterns popover: the ? button by the search bar toggles it; a click elsewhere
// or Escape closes it. Plain JS, so it works even without jQuery.
document.addEventListener('DOMContentLoaded', function () {
	var toggles = document.querySelectorAll('.search-help-toggle');
	if (!toggles.length) return;

	function panelOf(button) {
		return document.getElementById(button.getAttribute('aria-controls'));
	}

	function closeAll(except) {
		Array.prototype.forEach.call(toggles, function (button) {
			var panel = panelOf(button);
			if (panel && panel !== except) {
				panel.hidden = true;
				button.setAttribute('aria-expanded', 'false');
			}
		});
	}

	Array.prototype.forEach.call(toggles, function (button) {
		button.addEventListener('click', function (e) {
			e.preventDefault();
			var panel = panelOf(button);
			if (!panel) return;
			closeAll(panel);
			panel.hidden = !panel.hidden;
			button.setAttribute('aria-expanded', String(!panel.hidden));
		});
	});

	document.addEventListener('click', function (e) {
		if (!e.target.closest('.search-help, .search-help-toggle')) closeAll(null);
	});

	document.addEventListener('keydown', function (e) {
		if (e.key === 'Escape') closeAll(null);
	});
});
