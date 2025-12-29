/* StudioCommand shared shell JS (v0.1.85)
   - Populates elements marked [data-sc-version] with the running backend version
     (from /api/v1/status), when available.
   - Safe to include on landing/admin pages. */
(function(){
  const el = document.querySelector('[data-sc-version]');
  if(!el) return;
  fetch('/api/v1/status',{cache:'no-store'})
    .then(r => r.ok ? r.json() : null)
    .then(j => { if(j && typeof j.version === 'string') el.textContent = 'v'+j.version; })
    .catch(()=>{});
})();
