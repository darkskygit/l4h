# Render the user manual: make -C docs/manual  (needs TeX Live and latexmk)
l4h-manual.pdf: l4h-manual.tex
	latexmk -pdf -interaction=nonstopmode -halt-on-error l4h-manual.tex

clean:
	latexmk -C l4h-manual.tex

.PHONY: clean
