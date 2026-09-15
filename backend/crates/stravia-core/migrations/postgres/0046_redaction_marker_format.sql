UPDATE reversible_redaction_mappings
SET reference = '<!-- stravia-redaction-marker:rm_' || substr(reference, 17, 32) || ' -->'
WHERE length(reference) = 49
  AND substr(reference, 1, 16) = '~stravia-secret:'
  AND substr(reference, 49, 1) = '~'
  AND substr(reference, 17, 32) !~ '[^0123456789abcdef]';
