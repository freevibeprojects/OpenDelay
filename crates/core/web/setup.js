// SPDX-License-Identifier: GPL-3.0-only
// Script for the setup page (setup.html).
"use strict";
OD.loadLanguage().then(async () => {
  document.getElementById("overlay").textContent = `${location.origin}/overlay`;
  document.getElementById("dock").textContent = `${location.origin}/dock`;
  const [routes, st] = await Promise.all([fetch("/api/routes").then((r) => r.json()), OD.status()]);
  const box = document.getElementById("routes");
  for (const r of routes.data.filter((r) => r.enabled)) {
    const div = document.createElement("div");
    div.className = "route";
    const name = document.createElement("b");
    name.textContent = r.name;
    const url = document.createElement("code");
    url.textContent = `rtmp://127.0.0.1:${st.listener.port}/${r.path}`;
    div.append(name, url);
    box.append(div);
  }
});
