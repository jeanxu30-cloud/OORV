#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
manifest="${script_dir}/comparison_manifest.tsv"
summary_md="${script_dir}/comparison_summary.md"
summary_tex="${script_dir}/comparison_summary.tex"
paper_tex="${script_dir}/paper_fragment_fairness.tex"

if [[ ! -f "${manifest}" ]]; then
    echo "missing manifest: ${manifest}" >&2
    exit 1
fi

{
    echo "# Shared Fragment Comparison Summary"
    echo
    echo "Source manifest: \`comparison_manifest.tsv\`"
    echo
    echo "| System | Status | Source file | Lines | Signals | Functions | Constraints | Aux flattening decls | Identity explicit | Pair flattening | History | Activation | Executable | Alignment |"
    echo "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | --- | --- | --- | --- |"
    awk -F'\t' '
    NR == 1 { next }
    {
        printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n",
            $1, $2, $3, $4, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
    }' "${manifest}"
} > "${summary_md}"

{
    echo "% Auto-generated from comparison_manifest.tsv"
    echo "\\begin{tabular}{llllllllllllll}"
    echo "\\toprule"
    echo "System & Status & Source file & Lines & Signals & Functions & Constraints & Aux decls & Identity & Pair flattening & History & Activation & Executable & Alignment \\\\"
    echo "\\midrule"
    awk -F'\t' '
    NR == 1 { next }
    {
        printf "%s & %s & %s & %s & %s & %s & %s & %s & %s & %s & %s & %s & %s & %s \\\\\n",
            $1, $2, $3, $4, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
    }' "${manifest}"
    echo "\\bottomrule"
    echo "\\end{tabular}"
} > "${summary_tex}"

{
    echo "% Auto-generated from comparison_manifest.tsv"
    echo "\\small"
    echo "\\setlength{\\tabcolsep}{4pt}"
    echo "\\begin{tabular}{>{\\raggedright\\arraybackslash}p{0.15\\linewidth}>{\\centering\\arraybackslash}p{0.11\\linewidth}>{\\centering\\arraybackslash}p{0.11\\linewidth}>{\\centering\\arraybackslash}p{0.17\\linewidth}>{\\centering\\arraybackslash}p{0.18\\linewidth}>{\\centering\\arraybackslash}p{0.10\\linewidth}}"
    echo "\\toprule"
    echo "System & Signals & Aux decls & Pair handling & History & Match \\\\"
    echo "\\midrule"
    awk -F'\t' '
    NR == 1 { next }
    $2 == "placeholder" { next }
    {
        pairs = ($11 == "yes" ? "manual" : ($11 == "no" ? "quantified" : $11))
        align = ($15 == "approximate" || $15 == "approximate_source" ? "approx." : $15)
        printf "%s & %s & %s & %s & %s & %s \\\\\n",
            $1, $6, $9, pairs, $12, align
    }' "${manifest}"
    echo "\\bottomrule"
    echo "\\end{tabular}"
} > "${paper_tex}"

printf 'wrote %s\n' "${summary_md}"
printf 'wrote %s\n' "${summary_tex}"
printf 'wrote %s\n' "${paper_tex}"
